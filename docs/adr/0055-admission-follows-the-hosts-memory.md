# 0055. Admission follows the host's memory, and caps keep room for page cache

Status: **Accepted** (2026-10-24). Amends [0033](0033-measured-admission.md),
[0032](0032-sizing-advice-from-sampled-peaks.md) and [0039](0039-auto-sizing.md). Amended by
[0063](0063-a-warm-runners-room-pays-the-hosts-deficit-first.md): a stopped warm runner's room pays the host's
deficit first, and a seed container fits the budget as a runner does.

## Context

On the owner's host, runner downloads from GitHub stalled for up to 100 s. The network was fine. The kernel was
throttling the sockets: when a cgroup's reclaim goes badly (`vmpressure` above its low level), every socket in that
cgroup and its descendants keeps a receive window of a few segments, and `memory.events` counts each time in
`sock_throttled`. Reclaim goes badly in two cases:

- **Global reclaim on an oversubscribed host.** The host has 64 GB of RAM and no swap. The desktop peaked at 50 G
  while the slice held 30 G of a 46 G `MemoryMax`. Admission counted only the slice against its `MemoryMax`, which
  is fixed, so it kept starting runners while the rest of the host already used the RAM the budget assumed.
- **Limit reclaim at a runner's own cap.** Sizing set caps at the largest peak plus 15%, and peaks leave out page
  cache. A job near its peak then has little room left for page cache under the cap, and with no swap, reclaim can
  only drop page cache. A scratch container capped at 1 G, holding 820 M of anon memory while it churned page cache,
  counted 2,400 to 2,800 `sock_throttled` in 30 s, and a 13 MB download took up to 12 s instead of 0.9 s. Repeating
  that over caps of 1 G and 4 G showed the edge is an absolute amount, not a share: with 396 M or less of room
  above the anon memory, the container throttled; with 496 M or more, it never did, whatever the cap.

Turning off socket accounting on the kernel command line hides the symptom, but it asks every user for a
bootloader change and leaves the reclaim that slows the jobs. runnerd should prevent this itself, without a knob.

## Decision

- **The host's memory is read with the slice's.** Every 2 s the sampler reads `MemTotal` and `MemAvailable` from
  `/proc/meminfo`, `/proc/pressure/memory`'s `some` and `full` lines, and the slice's memory without page cache or
  reclaimable slab, which `MemAvailable` counts already (`api::HostMemory`). The reading is kept in memory and shown in the status and by `runnerd doctor`, not stored.
- **New runners fit what the host leaves the slice.** Beside `MemoryMax`, admission bounds what the slice may
  reserve by what its runners hold without page cache plus `MemAvailable`, less a margin: the larger of 2 G and a
  sixteenth of the host's memory, 4 G on 64 G (`admission::host`). Admission fits new runners' reservations into
  that less what the running ones reserve. Because the running ones count at the larger of what they reserved and
  what they use, a start the host cannot yet see in `MemAvailable` still counts. This is the same rule under both
  policies: under caps a runner reserves its cap, which is also the most it can hold. A job that waits for it says
  so: `Waiting::HostMemory`, "The host is short of memory: needs 6G; the host has 2G to spare".
- **Nothing starts while the host stalls short of memory.** When `full` pressure over the last 10 s reaches 5%
  while `MemAvailable` is under twice the margin, no runner starts. Starts resume only under 1%. The shares are the
  whole host's, and a runner reclaiming page cache at its own cap raises them too: one scratch container at its cap
  took them from 0 to 1.1% with 37 G available. With that much available, a stall is the runners' own limit reclaim,
  which the page cache room below addresses and starts elsewhere do not make worse.
- **Both bounds have hysteresis.** While a job waits for the host or the host stalls, and a runner runs, the margin
  grows by half, so starts resume a little past the edge, not at it. Otherwise each start would push the host back
  to the edge. With no runner in the slice there is no start to follow, so the first one needs only the margin.
- **Nothing running is stopped for it.** A host short of memory only holds starts back. Warm runners neither start
  nor stop to make room while the host stalls, as at the slice's high-water mark.
- **Those waits are not demand for slots.** Each plan adds the time since the last to every job that waited for the
  host then, on the job's row (`jobs.host_waited`, schema 13). Sizing and auto sizing take it out of the job's wait,
  so a host short of memory does not make auto sizing add slots, while a job that waited for a slot beside it still
  counts.
- **Caps keep room for page cache.** Sizing advises, and auto sizing writes, at least the largest peak plus 1 G
  (`api::PAGE_CACHE_ROOM`), or plus `[sizing] margin` when that is more. 1 G is twice the edge measured above.
  Measured admission's reservations stay at the peak plus the margin: page cache under a cap is the kernel's to
  drop, so it is not reserved against the slice.
- **What happened shows per job.** Each runner's `sock_throttled` goes into its row with its other counters, and
  then into the job's record (`JobMetrics::sock_throttled`), `runnerctl job` and the job page. A repository with two
  or more throttled runs in the window gets a warning insight, `throttled`, which notifications send.
- No config key. The thresholds are constants; a host that needs other numbers can get keys then.

## Consequences

- While the desktop takes most of the host's RAM, the slice may hold far less than its `MemoryMax`. Jobs wait, with
  the host named in their reason, instead of starting and stalling every running job.
- Small jobs get larger caps: a job that peaks at 300 M gets 1.5 G instead of 512 M. Under measured admission
  that costs no budget, since reservations follow peaks. Under caps, fewer slots fit `MemoryMax`, and auto sizing
  raises caps it set before toward the new advice.
- A host whose other programs hold memory for a long time keeps jobs waiting until they free it. Nothing makes a job
  start anyway, since a start then would slow every running job too.
- The margin and the stall thresholds are a judgement, not measured on many hosts. The `throttled` insight and the
  job page show when they are not enough.
- A kernel without pressure stall information still has `MemAvailable`. Without `/proc/meminfo` the host bound is
  absent, as without a recent reading, and only `MemoryMax` bounds starts.
- `runnerd doctor` reads the host by admission's rule with the plain margin, with what the slice's runners hold
  now, and warns while the host stalls short of memory or has less to spare than the smallest profile's `memory`.
  Admission keeps half a margin more after a wait and while a stall lasts, so the doctor can pass while admission
  still holds jobs. Under measured admission a runner may reserve less than `memory`, so the doctor can also warn
  where admission still starts one.
