# 0037. Memory is what the kernel cannot reclaim; a cap is met when the kernel says so

Status: **Accepted** (2026-10-23).

## Context

Samples counted `memory.current` less `inactive_file`, as `docker stats` does. That still counts `active_file`,
page cache read twice, which the kernel takes back under pressure as readily. On the owner's host one runner held
3.23G by `memory.current`; runnerd said 1.55G, of which 0.96G was `active_file`, and anonymous memory, tmpfs and
kernel memory came to 0.55G. A runner at its 6.25G cap showed 3.62G against 3.40G it could not give back.

Those samples are job peaks, sizing advice ([0032](0032-sizing-advice-from-sampled-peaks.md)) and measured
admission's predictions ([0033](0033-measured-admission.md)), and a peak within 5% of its cap counted as the job
meeting its cap. A job whose build reads its tree twice fills its cap with page cache, so `web-verify`, 1.3G a day
earlier, peaked at 5-6G and reserved its whole 6.25G cap every run.

The kernel's cgroup v2 documentation (`Documentation/admin-guide/cgroup-v2.rst`) gives the terms:

- `memory.max`: "If a cgroup's memory usage reaches this limit and can't be reduced, the OOM killer is invoked".
  What the kernel can reduce is not what kills a job.
- `memory.stat`: `file` is page cache "including tmpfs and shared memory"; `shmem` is the swap-backed part of it,
  and "shmem pages are on anon memory management lists", so `active_file` and `inactive_file` hold only page cache
  the kernel can drop or write back. `slab_reclaimable` is the "part of slab that might be reclaimed, such as
  dentries and inodes"; `slab_unreclaimable` "cannot be reclaimed on memory pressure".
- `memory.events`, hierarchical: `max` counts every time usage "was about to go over the max boundary", which page
  cache does all the time; `oom` counts when usage "reached the limit and allocation was about to fail", after
  reclaim failed; `oom_kill` counts processes killed for memory.

## Decision

- A sample's memory is `memory.current` less `active_file`, `inactive_file` and `slab_reclaimable`: anonymous
  memory, tmpfs and shared memory, and kernel memory the kernel cannot reclaim, the same for a container and for
  the slice. No share of page cache is added back: a job needs some to run fast, and the sizing margin leaves it
  that; too little shows as a slower job, while a cap below what it cannot give back kills it.
- The slice's high-water mark (90% of `MemoryMax`) uses the same number. The kernel reclaims page cache before it
  kills anything, so a slice full of page cache has room; holding admission for it would stop starts for nothing.
- A run met its cap when the kernel killed a process in its container for memory (`oom_kill`, or Docker's
  `OOMKilled`) or its container's `memory.events` counted an `oom`. Both counters are sampled with the container's
  memory; `runners.ooms` keeps the largest `oom` count. Only a run whose container's events were never read falls
  back to a peak within 5% of its cap. That rule decides when admission puts a job back on its cap for 7 days and
  when sizing refuses to lower a cap; a kill now also keeps sizing from lowering one.
- Schema version 8 adds `runners.memory_measure`, `'unreclaimable'` for runners started from it, and
  `runners.ooms`. Runners from before have neither.
- The daemon's predictor reads only runs measured the new way. Old peaks overstate what jobs need; kept, they would
  hold a job like `web-verify` at its cap until they left the 30-day window. Without them a job reserves its cap,
  or its profile's largest peak once the profile has `[sizing] min_jobs` new runs, which is what admission by caps
  does anyway. Sizing advice and `runnerd simulate` keep the old rows, with the 5% rule: they only make advice and
  replays err toward more memory, and leaving them out would leave a month without either.

## Consequences

- Peaks, the dashboard's and TUI's memory charts and the slice's memory read lower than before, by the page cache
  a job used, and lower than `docker stats`.
- After the upgrade measured admission reserves caps until runs measured the new way build up again: about
  `min_jobs` runs per profile.
- A run killed between two samples of `memory.events` is still caught by Docker's `OOMKilled` when the container
  stops; an `oom` without a kill between the last sample and the container's removal is missed.
- A `memory.stat` without `active_file`, `inactive_file` or `slab_reclaimable` gives no samples, as one without
  `inactive_file` did before.
