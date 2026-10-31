# 0090. Under measured admission, slots follow waits for a slot alone and Docker's disk

Status: **Accepted** (2026-10-31). Amends [0039](0039-auto-sizing.md) and
[0049](0049-auto-sizing-without-hand-tuning.md), where they say when auto sizing adds slots under `measured`, and
[0054](0054-slot-docker-stores-share-the-disk.md), whose share now also bounds how many slots auto sizing adds.

## Context

The owner's host ran `admission = "measured"` with `[sizing] auto = true` for 8 days and 2,611 jobs. Every profile
reached its slot count in every burst (light 4 of 4, jvm 4 of 4, node 10 of 10). From queued to runner start, cold
jobs waited p50 59 to 108 s and p90 3 to 16 minutes, while slots were busy 1 to 6% of the time and CPU reservations
peaked at 44.3 of 48. From runner start to job start took 5 s. Auto sizing never added a slot, and the owner raised
jvm to 6 and light to 8 by hand.

[0049](0049-auto-sizing-without-hand-tuning.md) added a slot under `measured` only while every profile's slots at
their jobs' p95 peaks fit `MemoryMax`: jvm at 9.47G × 5 alone passes the 46G slice, node at 7.05G × 10 is 70G. That
is the caps rule with a smaller number. Under measured admission a slot costs no memory: each runner reserves its own
job's prediction when it starts, and the slice budget and the host check stop a start that does not fit. The p95
peak is the heaviest job's, and every slot of every profile running its heaviest job at once is not what admission
plans for.

The trigger was wrong as well. A wait counted whatever held the job back: the slot, the slice's memory or the CPU
budget (only the host's memory was taken out, [0055](0055-admission-follows-the-hosts-memory.md)). `NoSlot`, as the
plan labels a wait, says no slot was free; the plan returns it before it checks the budgets, so a job memory would
also hold back is labelled `NoSlot` too. Counted as is, a burst of 10 jvm jobs with memory for 2 more would add slots
until something else stopped it, and the jobs would then wait for memory instead. And under `measured` a slot busy
half the time with no job waiting starts nothing sooner.

What a slot does cost is disk. Every slot's Docker store shares the same quarter of Docker's disk
([0054](0054-slot-docker-stores-share-the-disk.md)); each new slot shrinks every slot's share and adds a volume. A
share smaller than what a store holds empties it after each job, which trades the wait for a cold build.

## Decision

### A wait for a slot alone

- The plan says which jobs one more slot would start now (`Plan::slot_waiting`): no slot of their profile is free or
  a warm runner's to give, the profile is not reseeding or pulling its image, and the memory, host and CPU budgets,
  less what the older of these jobs would take, have room for their runners. Each counted job takes its runner from
  a copy of what is left, so no more are counted than the budgets would start. Whatever the wait's label says,
  `NoSlot` or `BehindStart`, only that decides.
- The engine adds the time since the last plan to each such job's `slot_waited` after every plan, as it adds
  `host_waited`. Kept per job in the history, it survives restarts and reloads; a job a restart left waiting counts
  again from the first plan after it. Schema 18 adds the column; rows from before it count as no wait for a slot, so
  the first raise waits for fresh evidence. 0.0.25 refuses a schema-18 history as newer than it reads.
- `ProfileSizing::slot_wait` is the spread of that figure. `wants_slots`, auto sizing's trigger and the advice line
  ("jobs waited 300s for a slot (p95) while memory and CPU had room") read it, under either admission policy.
  `ProfileSizing::wait`, the whole wait less the host's part, stays what clients show as a profile's wait.

### Adding slots under measured admission

- Only waits for a slot alone add one: 3 jobs at least since the profile's last change, a minute or more at p95. A
  busy share no longer does under `measured`. Under `caps` the rule stays: a wait or a busy share adds one slot while
  every cap fits `MemoryMax`.
- A raise adds as many slots as jobs waited for one at once since the last change, at least 1 and at most `max_step`
  percent of the slots (50 by default), up to `max_slots`. Each job's wait for a slot is taken as ending at its
  start; how many overlap is the deficit. One slot an hour was too slow for this host: jvm went 4 to 6 and light 4 to
  8, and bursts come every few hours, so +1 a raise would take days of bursts. At 50%, jvm gets its 6 in one raise
  and light its 8 in two.
- The bound is disk, not memory: the slots that leave every slot's share of Docker's disk (`docker_budget_gib` of
  the measured total, among every profile's slots with this change and the ones before it in the look) at least the
  floor. The floor is the largest median slot volume of any profile at the last measure, since every slot's share
  shrinks, at least 4G and at most 20G, the most a share can be. 4G sits under the 6 to 7G that
  [0054](0054-slot-docker-stores-share-the-disk.md) found enough for a slot's images and Testcontainers' images, so
  stores that keep little do not hold slots back, while stores that keep more raise the floor to what they keep.
  Docker's disk not measured yet, as just after a start, adds no slot. This replaces "without `max_slots` and a
  `MemoryMax`, none is added".
- CPU: with `cpu_overcommit` set, admission guards CPU, and a job the CPU budget would hold back is not a wait for a
  slot alone. Without it there is no CPU budget, as before: more slots run more jobs on the same cores, each with its
  CPU weight ([0079](0079-cpu-weights-and-throttling-as-evidence.md)), which slows them but does not stop them, and
  the jobs no longer wait.

### Taking back idle slots

Raises are now easy, so slots that stay idle are taken back, one at a time: an idle slot keeps its volume and makes
every slot's share smaller.

- A profile loses one slot when, for the week before the look, the most of its runners that held a slot for a job at
  once, plus its warm runners, stayed 2 or more under its slots. One under would take back a slot the next burst
  adds again.
- Only once the history holds a runner from before that week and the profile has had its slots for all of it: by its
  `profile.changed` events, a hand edit, reload or undo starts the week again, and by auto sizing's own last change.
  At least `min_jobs` of its jobs ran in the week, so a host that was off says nothing. Never under what runs or 1;
  the write still refuses a config without a slot a runner is in.
- It reads runners over a week, not which slot index ran last: a job returns to the slot that ran it
  ([0080](0080-a-job-starts-in-the-slot-that-ran-it-last.md)), so the highest index used stays high under light load.
- This takes back slots set by hand too, such as the owner's jvm 6 and light 8, once they stay idle for a week. That
  is meant: sizing slots by hand is what auto sizing replaces. `max_slots` bounds raises; nothing but the idle rule
  lowers.

## Consequences

- A burst that only slots hold back adds slots within `raise_interval`, sized to the burst, as far as the disk's
  floor allows; one that memory or CPU holds back adds none and waits as before.
- The next check of the caches removes a lowered slot's volume ([0054](0054-slot-docker-stores-share-the-disk.md)),
  and a new slot starts with an empty layer and Docker store: its first jobs build cold.
- Warm runners do not grow with slots: each pool's `warm` is a count, not a share. Reseeds read slot 1 as before.
- `runnerd simulate --slots` still replays the history with more slots, for a profile auto sizing does not size.
