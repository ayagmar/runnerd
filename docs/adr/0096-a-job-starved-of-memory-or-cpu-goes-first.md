# 0096. A job starved of memory or CPU goes first

Status: **Accepted** (2026-11-06). Amends [0033](0033-measured-admission.md),
[0038](0038-cpu-admission.md), [0044](0044-queued-jobs-say-why-they-wait.md), [0050](0050-warm-runners.md),
[0055](0055-admission-follows-the-hosts-memory.md), [0056](0056-adopted-runners-keep-their-reservations.md),
[0058](0058-a-repository-served-by-several-profiles.md),
[0063](0063-a-warm-runners-room-pays-the-hosts-deficit-first.md),
[0090](0090-slots-follow-waits-for-a-slot-alone-and-dockers-disk.md),
[0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md). The wire follows
[0061](0061-the-previous-release-reads-the-wire.md) and client parity [0062](0062-client-parity-is-checked.md).

## Context

`scheduler::plan` serves jobs in job-ID order, but a job whose runner does not fit is skipped and the loop goes on to
younger jobs. Nothing limits how often that happens: slots bound how many run at once, not how many pass a waiting job.
Smaller runners can take every byte that frees up, so the room a large runner needs never gathers. CPU had a rule
against this (0038); memory had none.

The owner's status at 02:40:17 UTC on 2026-11-06 (the time of `budget.host.memory.at`):

- Slice 46 GiB, 42.75 charged, 3.25 free; host limit 44.515 GiB, 1.765 spare. Neither interlock active. CPU 24.3 of 48.
- trellis/jvm: 6 queued, runner 11 GiB, waiting 39.5 to 61.5 minutes, with 3 of the profile's 6 slots free.
- fretboard/jvm-heavy: 6 queued, runner 11 GiB, waiting 7.2 to 29.1 minutes, 3 of 3 slots free.
- fretboard/jvm: 35 queued, runner 7.75 GiB, waiting 7.2 to 34.7 minutes, itself short of memory by then.

Counting runner starts of strictly later-queued, larger-ID jobs before the snapshot, 307 passed the oldest trellis job
and 139 the oldest heavy job. These are local runner starts, not GitHub's assignments. A completed heavy job waited
76m26s and ran 2m04s, with 297 such starts before its runner's.

The bypass explains part of these waits. The rest was the host being short: the oldest trellis job has 25.67 minutes of
`host_waited`, the oldest heavy one 6.80. This record does not fix a short host.

## Decision

One rule for memory and CPU: once a pool's next job has waited long enough for what it lacks, and draining the running
jobs would make room for it, no younger job starts until a runner of that pool does. It is on under both admission
modes, has no config key and persists nothing, so the schema does not change.

### The barrier

`scheduler::priority` holds the rule; `plan` and `admission::simulate` both call it. A plan has at most one barrier.
Its owner is the first job in the plan's job-ID order that has a free slot, lacks memory or CPU, has waited long enough
for either, and fits what the budgets would leave drained. Paused, backing-off and preparing pools are not in the
running, nor is a pool with no free slot. Jobs older than the owner keep their place. Jobs behind it wait.

Ownership is decided again by every plan, not kept. Two pools each needing what the other uses cannot deadlock, because
only the older one owns the plan; it drains towards everything it lacks, as nothing younger starts. A job that waited
for CPU and now lacks memory, or the reverse, keeps what it waited, and its current shortage names the reason younger
jobs give, memory before CPU as `Waiting` already orders them. This replaces 0038's separate CPU lock.

`MEMORY_PRIORITY_AFTER` is its own constant, 120 s, equal to `CPU_PRIORITY_AFTER` for now. They are separate because
memory and CPU are measured differently and may need to move apart. It is not a config key: an owner cannot be expected
to tune it (the opposite of what auto sizing is for), and the replay below gives no reason to want it off.

### Waits

`RepositoryState::waits` keeps `{job, since}` per pool, for memory and for CPU. `plan` names the job
(`Plan::held`): the pool's first job that lacks a resource, past those its idle runners take and those it starts a
runner for, from the demand `listed_demand` leaves after taken, consumed and unnamed claims. The engine does not work
it out again from queue length.

- Commits happen in `note_waits`, after the last plan of a tick and after each pass's plan (0095). `publish` also
  plans, and never reaches it, so reading the status changes no wait.
- A wait counts for its own job only, so a wait left over from a job gone cannot promote the next one.
- Holding the job for a slot or for the other resource does not make it younger.
- A runner in its slot ends the pool's waits. A proposed start, a deferred one, or a start that failed preparing the
  caches does not: that failure leaves no runner and no backoff, and ending the wait there would make the job young
  over and over.
- A listing that succeeded and no longer shows the job, or a claim on it, ends the wait.
- A listing that failed keeps it. The plan has no such job, so no barrier stands for it and other repositories go on;
  the next listing that succeeds resumes the age if the same job is still there.
- Pausing, a failure backoff, and a reseed or image pull end the wait. A broken pool then cannot hold everyone for a
  start that fails, and a pool that comes back waits the limit again.
- A reload prunes the waits of pools gone or moved to another profile. What a runner needs is read again every plan.
- A restart loses every wait. No stale state can hold a queue, and a job that was starved holds again the limit after
  the first plan. Frequent restarts inside the limit would keep it from ever holding.

Each runner a pool starts ends its wait, so the pool's next job starts its own limit. Six jobs of one pool take turns
with what younger jobs do in those 2-minute windows; they do not hold one barrier for all six.

### What may hold

`Budget::memory` and `Budget::host` are what is left, signed, not totals, so `plan` cannot tell from them whether a
job could ever fit. The engine passes `Budget::drained`:

- Slice: `MemoryMax` less `committed_memory`, which is the mirror, seed containers, the room a waiting reseed holds and
  shells. Every runner counts as draining at what `takes` charges, adopted and retired ones too, so a stale sample that
  charges a runner at its cap changes nothing here.
- Host: `host::limit` with the plain margin, the one the first runner into an empty slice goes by, less the same
  commitments. A host short because of programs outside the slice fails this bound.
- CPU: the whole budget, as nothing else reserves CPU.

A demand must fit every known bound; a bound not known restricts nothing. With neither a memory budget nor a host
reading nothing is ever short of memory, and the rule is inert. A demand no drain could serve keeps its `Memory` or
`HostMemory` reason and erects nothing, so younger jobs that fit keep starting. It may still age, and holds if the bound
later moves, such as when programs outside the slice exit.

The high-water mark and a host stall put zeros into `memory` and `host` that say nothing of the room there is
(`Budget::held`). While either applies, no barrier stands, no new wait starts, waits keep their age and no warm runner
stops. Nothing starts then anyway. `Budget::held` keeps its meaning; `Waiting::reason` still reads it as the high-water
mark, and it is not a reservation for a starved job.

### What it promises

While a demand stays valid, once it is past the limit no younger admission uses its opportunity; once what was admitted
earlier has drained and the budgets and interlocks allow it, the next plan starts a runner for that pool. That holds if:
listings and samples resume after a failure, running work ends and idle runners go, nothing pauses, prepares or backs
the pool off for good, its needs stay feasible, and its registration and start succeed. A hung job or memory pressure
outside the slice defeats it. The tests prove the bound with bounded fake run times; a finite past run is no bound on a
future one.

The promise is to a pool. A runner registers for a pool and GitHub may give it any job the labels match (0058), so this
supplies no fairness among one pool's jobs.

### Next to what already runs

- Runners already admitted are never stopped, clamped or lowered for a barrier. A waiting runner's truthful raise
  (`rereserve`), samples above a reservation, and 0033's over-budget stop go on as before. The barrier governs new
  admissions only.
- Idle younger runners are not reclaimed beyond today's rules. They leave at `IDLE_TIMEOUT` or by taking a job, which is
  one of the assumptions above.
- Warm runners: none starts while a job is behind a barrier (`start_warm` knows the new reason). A job behind a barrier
  never reclaims warm room. The owner reclaims by `Warm::make_room` as before, all or nothing, repaying a signed deficit
  first (0063), and only for an immediate start; a warm runner that took a job or failed to go stays charged. Under an
  interlock none stops, as `Warm::new` already says.
- A start planned before the barrier and refused by `still_fits` keeps its wait and plans again.
- A pass runs only starts that replace no warm runner. If the owner's start needs warm room it waits for the tick, and
  a younger start in that plan was already counted beside it, so the pass takes none of its room.
- Seed containers and shells are not held. They change `drained`, which the next plan reads.

### What jobs say

`Waiting::BehindMemory { job }` (`behind-memory`) is `WaitKind::Full` beside `BehindCpu`, in core's words: "Behind
lint, which waits for memory". It means an active barrier, not an aged job. The owner keeps its direct reason, or
`BehindStart` where only this plan's starts took its room, which still tells a status plan's hypothetical starts from a
real shortage. A paused, backing-off, preparing or unserved job keeps its reason, and so does one with no slot: that is
what holds it.

Jobs behind a barrier are never `Plan::slot_waiting`, whether or not they have a slot: another slot would not start
them, so auto sizing reads no slot wait from a barrier (0090). `host_waited` still counts direct `HostMemory` reasons
only, so a job behind a host-short owner adds nothing to it, and 0055's larger margin follows the owner's reason.

A job behind a barrier that itself lacks memory or CPU is in `Plan::held` too and ages there, so it can go next rather
than start its limit when the barrier falls. It shares no barrier.

A client of the previous release reads `behind-memory` as `Waiting::Unknown` and says it waits for a reason it does not
know. `ACCEPTED` in the wire check names that one item, since the check counts reading a variant as the fallback as a
failure, and a test asserts the fallback. The sample stays. No budget fact was added.

### Why strict, with no backfill

Letting a younger job start when its usual run time ends before the room could gather was considered and left out.
Usual run times are not deadlines, a running job can overrun, and a younger runner may take any job of its pool, so it
could not promise "would not delay". It would need elapsed and remaining times and confidence rules that `Profile` and
`RunnerSlot` do not carry: a prediction subsystem inside a fix.

A later backfill that is safe needs no prediction: admit only what fits beside the owner's whole protected need, that
is, while `left` less the owner's memory and CPU still has room. It stays strict about the owner and gives back surplus
above it. Guessed completion times would be a separate, openly probabilistic design.

### Replay

`runnerd simulate` now goes by the same `Barrier::of` and `Waits`, in job-ID order, and moves at each wait's limit as
well as at arrivals and ends, so a quiet queue reaches it. `--priority` replays the config's policy and slots without
and with the barrier, pool by pool and profile by profile. Busy share is what the running jobs were charged over time,
divided by the budget over time. The replay has no host readings, warm runners, restarts or recoveries, and each job
gets the runner started for it.

The backup of the owner's history, last 6 hours, 46 GiB, the configured slots, measured:

| Pool | Jobs | Wait p50 / p95 / max without | With |
|---|---:|---|---|
| fretboard/jvm | 491 | 2m19s / 11m05s / 22m41s | 2m19s / 11m05s / 22m41s |
| trellis/jvm | 26 | 45s / 6m10s / 6m29s | 52s / 5m56s / 5m56s |
| fretboard/node | 487 | 6m07s / 21m14s / 30m11s | 6m28s / 21m10s / 30m27s |
| fretboard/light | 1,495 | 0s / 36s / 1m04s | 0s / 3m51s / 10m34s |
| trellis/light | 27 | 0s / 32s / 32s | 0s / 2m09s / 6m38s |

A barrier stood for 24m59s with 152.5 GiB-minutes free meanwhile; the busy share went from 61.0% to 60.9%. This says
nothing of the twelve starved jobs: they are still queued, have no run time, and are among the 82 queued jobs the replay
leaves out. Their p95 is not claimed. What it does show is the price: light jobs, which never needed the room, wait
out barriers they could have fit beside.

A deterministic workload shaped like the snapshot, with **assumed** run times (three hours of arrivals, caps, node 24
minutes, mid 10, light 2, and twelve 11 GiB jobs running 5, 10 or 20 minutes in three runs): p95 of the large pool fell from 14m56s to 9m57s,
29m55s to 19m55s and 49m55s to 29m55s, and of the heavy pool from 29m56s to 19m55s, 59m55s to 34m55s and 2h01m to
59m55s. The small jobs pay more the longer the large ones run: light p95 from 9m30s to 13m30s, 6m30s to 29m30s, and
13m18s to 59m00s; node from 10m48s to 15m00s, 7m54s to 32m00s, and 14m24s to 59m36s. Twelve large jobs are a sustained
overload there, not a typical hour.

## Consequences

A large job no longer loses to an endless stream of smaller ones. Small jobs pay: while a barrier stands, memory sits
free that one of them could have used. In the owner's last 6 hours that took light's p95 from 36s to 3m51s, and under a
sustained run of large jobs it costs far more. The surplus backfill above is the way to give that back if it proves too
dear.

ADR 0038's "`runnerd simulate` does not model this priority" no longer holds: it does, for both resources. Its
statement that a held job's wait ends when a runner of the repository starts now means the runner is in its slot, and
the wait also ends on pause, backoff and preparation.
