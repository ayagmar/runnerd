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
([0054](0054-slot-docker-stores-share-the-disk.md)); each new slot shrinks every slot's share and adds a volume.

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
- `ProfileSizing::slot_wait` is the spread of that figure. Under `measured`, `wants_slots`, auto sizing's trigger and
  the advice line ("jobs waited 300s for a slot (p95) while memory and CPU had room") read it.
  `ProfileSizing::wait`, the whole wait less the host's part, stays what clients show as a profile's wait, and what
  asks for slots under `caps`: a slot there is bought with its cap, and once every slot is busy and the caps fill
  `MemoryMax`, no job has room for its runner, so none would count as a wait for a slot alone.

### Adding slots under measured admission

- Only waits for a slot alone add one: 3 jobs at least since the profile's last change, a minute or more at p95. A
  busy share no longer does under `measured`. Under `caps` the rule stays as it was: a wait or a busy share adds one
  slot while every cap fits `MemoryMax`.
- A raise adds as many slots as jobs waited for one at once since the last change, at least 1 and at most `max_step`
  percent of the slots (50 by default), up to `max_slots`. Each job's wait for a slot is taken as ending at its
  start; how many overlap is the deficit. One slot an hour was too slow for this host: jvm went 4 to 6 and light 4 to
  8, and bursts come every few hours, so +1 a raise would take days of bursts. At 50%, jvm gets its 6 in one raise
  and light its 8 in two.
- The bound is disk, not memory: slots go up only while every slot keeps 2G (`api::SLOT_DISK`) of the quarter of
  Docker's disk the slots' stores may keep together (`docker_budget_gib` of the measured total, among every profile's
  slots with this change and the ones before it in the look), and never past `max_slots`. Docker's disk not measured
  yet, as just after a start, adds no slot. This replaces "without `max_slots` and a `MemoryMax`, none is added".
- What the stores hold does not bound slots, because a store fills the share it has. On the owner's host the slots'
  stores may keep 79G together, 4G a slot up to 18 slots, 3G up to 24. Over a week the slot volumes, measured after
  each job's cleanup, held at p50 / p90 / max: jvm 2.2 / 3.5 / 4.2G, node 0.7 / 2.8 / 8.2G, light 0.5 / 0.8 / 1.4G.
  jvm's stores fill their 4G share; one measured 4.2G. A floor of a week's p90, jvm's 3.5G, holds the host at 18 slots,
  under the 24 the owner set by hand; a floor of one measure's median swings between an emptied store and a full
  one. The sizes follow the share, so such a floor follows the slot count it was measured at.
- So the bound is the least share at which those stores keep their images: 2G, 36 slots on the owner's host, 12 more
  than the 24 it runs. Raises are sized to the bursts that waited, and the trigger needs memory and CPU room, but a
  week's bursts overlapped up to 11 jvm, 20 light and 14 node jobs: had all of that been a wait for a slot alone,
  the profiles would take about 69 slots, so in practice the share, not the trigger, is what stops them. Its cost is
  in the consequences. A raise's reason and a refusal say how the share drops, as `each slot keeps 2G of Docker's
  disk, down from 3G`.
- A raise the jobs ask for and auto sizing cannot make, at `max_slots`, before the measure or at the 2G share, is a
  refusal (`AutoPlan::refused_slots`), which clients show as `No slot raise: …` beside the profile's next change, as
  they show a `cpus` raise the CPU budget refuses. The advice line then says auto sizing cannot add one now, instead
  of promising it.
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
  `profile.changed` events, a hand edit, reload or undo starts the week again, and so does auto sizing's last change
  of its slots. A change of its memory or `cpus` does not: on the owner's host jvm and node had one every few days.
  At least `min_jobs` of its jobs ran in the week, so a host that was off says nothing. Never under what runs or 1;
  the write still refuses a config without a slot a runner is in.
- It reads runners over a week, not which slot index ran last: a job returns to the slot that ran it
  ([0080](0080-a-job-starts-in-the-slot-that-ran-it-last.md)), so the highest index used stays high under light load.
- This takes back slots set by hand too, such as the owner's jvm 6 and light 8, once they stay idle for a week. That
  is meant: sizing slots by hand is what auto sizing replaces. `max_slots` bounds raises; nothing but the idle rule
  lowers.

## Consequences

- A burst that only slots hold back adds slots within `raise_interval`, sized to the burst, up to `max_slots` and the
  2G share; one that memory or CPU holds back adds none and waits as before.
- More slots mean smaller shares, and the job cleanup (`image/job-cleanup.sh`) holds each store to its share after
  every job, reading its size in whole GiB rounded up. A store over its share loses its build layers, then its least
  recently used cache mounts, then its images, then everything. On the owner's host, with stores at p50 / p90 of jvm
  2.2 / 3.5G and node 0.7 / 2.8G:
  - up to 18 slots each keeps 4G, and the stores keep what they hold now;
  - 19 to 24 slots keep 3G: jvm's fuller stores lose build layers after a job;
  - 25 to 36 keep 2G, the most auto sizing reaches: jvm stores lose their build layers after most jobs and node's
    fuller ones after some, but they mostly keep their images, so a job pulls little and builds colder;
  - 37 to 73 would keep 1G, the least a share can be: every store over 1G, most jvm and many node ones, is emptied
    after each job, images included, and every job there starts cold. That is why the bound is 2G, not 1G.
- The next check of the caches removes a lowered slot's volume ([0054](0054-slot-docker-stores-share-the-disk.md)),
  and a new slot starts with an empty layer and Docker store: its first jobs build cold.
- Warm runners do not grow with slots: each pool's `warm` is a count, not a share. Reseeds read slot 1 as before.
- `runnerd simulate --slots` still replays the history with more slots, for a profile auto sizing does not size.
