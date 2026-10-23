# 0037. A job's memory is estimated without page cache; a cap is met when the kernel says so

Status: **Accepted** (2026-10-23).

## Context

Samples counted `memory.current` less `inactive_file`, as `docker stats` does. That still counts `active_file`,
page cache read twice, which the kernel drops under pressure as it does the inactive part. On the owner's host one
runner held 3.23G by `memory.current`; runnerd said 1.55G, of which 0.96G was `active_file`, and anonymous memory,
tmpfs and kernel memory came to 0.55G. A runner at its 6.25G cap showed 3.62G, 3.40G of it outside page cache.

Those samples are job peaks, sizing advice ([0032](0032-sizing-advice-from-sampled-peaks.md)) and measured
admission's predictions ([0033](0033-measured-admission.md)), and a peak within 5% of its cap counted as the job
meeting its cap. A job whose build reads its tree twice fills its cap with page cache, so `web-verify`, 1.3G a day
earlier, peaked at 5-6G and reserved its whole 6.25G cap every run.

The kernel's cgroup v2 documentation (`Documentation/admin-guide/cgroup-v2.rst`) gives the terms:

- `memory.max`: "If a cgroup's memory usage reaches this limit and can't be reduced, the OOM killer is invoked".
- `memory.stat`: `file` is page cache "including tmpfs and shared memory"; `shmem` is its swap-backed part, and
  "shmem pages are on anon memory management lists", so `active_file` and `inactive_file` hold page cache that
  reclaim can drop or write back, though dirty, writeback or pinned pages may not go at once. `slab_reclaimable`
  is slab that "might be reclaimed, such as dentries and inodes": an accounting class, and a dentry or inode in
  use (an open file's) is not freed (`fs/dcache.c`).
- `memory.events`, hierarchical: `max` counts every time usage "was about to go over the max boundary", which page
  cache filling a cap does all the time; `oom` counts when usage "reached the limit and allocation was about to
  fail", after reclaim failed; `oom_kill` counts processes killed for memory.

## Decision

- A container's memory is an estimate of what its job holds without page cache: `memory.current` less
  `active_file` and `inactive_file`. That keeps anonymous memory, tmpfs and shared memory, and all kernel memory,
  reclaimable slab included, since in-use slab can't be freed. It is resident memory only: what is swapped out is
  not in `memory.current`. No share of page cache is added back: a job needs some to run fast, and the sizing
  margin leaves room for it; too little shows as a slower job, while a cap below what the job holds kills it.
- The slice's memory, which the high-water mark (90% of `MemoryMax`) compares, stays `memory.current` less
  `inactive_file`. It is the conservative number: active page cache that may not be dropped at once still counts,
  so the mark stops starts while there is still room rather than after the slice runs short.
- A run met its cap when the kernel killed a process in its container for memory (`oom_kill`, or Docker's
  `OOMKilled`) or its container's `memory.events` counted an `oom`. Both are sampled with the container's memory;
  `runners.ooms` keeps the largest `oom` count. A counter `memory.events` lacks, or that doesn't parse, is
  unknown, not 0: a run without a recorded `oom` counter falls back to a peak within 5% of its cap. Since the
  counters include nested containers, an `oom` at a smaller limit inside the runner counts too, which errs toward
  the cap. That rule decides when admission puts a job back on its cap for 7 days and when sizing refuses to lower
  a cap; a kill now also keeps sizing from lowering one.
- Schema version 8 adds `runners.memory_measure`, `'without_page_cache'` for runners started from it, and
  `runners.ooms`. Runners from before have neither.
- The daemon's predictor learns peaks only from runs measured the new way. Old peaks overstate what jobs need;
  kept, they would hold a job like `web-verify` at its cap until they left the 30-day window. An old run killed for
  memory still puts its job on its cap until 7 days after the kill: a kill says the cap was too small whatever the
  measure. An old peak near its cap or past what was reserved does not, since page cache may have made it.
- Sizing advice and `runnerd simulate` keep the old rows, with the 5% rule: they only make advice and replays err
  toward more memory, and leaving them out would leave a month without either.

## Consequences

- Peaks and the job memory charts in the dashboard and TUI read lower than before, by the page cache a job used,
  and lower than `docker stats`. The slice's chart reads as before.
- After the upgrade, a job without runs measured the new way reserves its cap, or its profile's largest peak plus
  the margin once the profile has `[sizing] min_jobs` new runs. That is less than its cap: a heavy job not seen
  since the upgrade can be under-reserved, as a new workflow can, and its cap still stops it.
- A run killed between two samples of `memory.events` is still caught by Docker's `OOMKilled` when the container
  stops; an `oom` without a kill between the last sample and the container's removal is missed.
- A `memory.stat` without `active_file` or `inactive_file` gives no samples.
