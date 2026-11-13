---
title: How big should each profile be?
description: The sizing advice runnerd gives from your jobs' peaks, auto sizing that applies it, and splitting light jobs from heavy ones.
---

`runnerctl sizing`, the dashboard's Sizing page and the TUI turn each job's recent peaks into advice for each profile's `memory` and `slots`. A job's recent runs are its last 60 successful ones in 30 days (3 × `min_jobs` when that is more than 20), failed ones included.

- The need is the largest peak plus `margin`, at least 1G for the page cache peaks leave out, rounded up to 256M. It follows the heaviest run, not a percentile: the cap kills a job that goes over. Under `min_jobs` jobs there is no advice.
- A run met its cap when its container's own limit ran out of memory (`oom` in `memory.events.local`); it counts at least its cap. A peak near the cap without `oom` is not a need for more.
- A job with such a run, or one killed for memory, stays on its cap until 3 runs in a row succeed clear of it; the profile is not lowered meanwhile.
- The advice lowers `memory` when the need is a tenth or more under it. Under `caps`, if jobs waited a minute or more (p95, [how a wait is counted](#how-a-wait-is-counted)) or the slots were busy half the time, it also adds as many slots as the memory now holds.
- Under `measured`, advice never changes `slots`; the advice line says when more would help.

[`[sizing]`](../../reference/configuration-more/#sizing) sets `margin` and `min_jobs`. Advice is written only when applied: `runnerctl sizing apply <profile>`, `a` in the TUI, or the Sizing page's button. It goes through the same check as any config and keeps to the profile's `min_memory`, `max_memory`, `max_slots` and [`memory_ceiling`](../../concepts/admission/#a-host-whose-memory-grows). `runnerd doctor` warns when a profile's jobs need more than its `memory`.

```console wrap=false
$ runnerctl sizing
[...]
node  7G × 2, 8 CPUs; 242 jobs, slots busy 78%, waited p50 12s p95 4m 20s
  warm runners took 187 of 242 jobs: waited p50 2s, the others 21s
  memory p50 1.4G p95 1.9G p99 2G max 2.1G of 242 recent runs
  cpu    p50 3.1 p95 6.9 p99 7.6 max 7.9 of the 242 at 8 CPUs
  lower memory to 3.25G from 7G, its largest peak + 1G for page cache; 4 slots fit in the same 14G
  Next look, within 5 minutes: memory 7G → 5.25G, slots 2

  REPOSITORY          JOB         RUNS   P50   P95  P99   MAX  CPU P99  WAIT P95  NEEDS
  halyard/storefront  CI / build   236  1.4G  1.9G   2G  2.1G      7.6    4m 20s  3.25G
  halyard/storefront  CI / e2e       6  1.8G    2G   2G    2G      7.6    4m 40s  max 2G
```

Peaks cover each job's recent runs, CPU peaks only runs at the profile's current `cpus`. The demo runs auto sizing with `interval = "3d"` and `max_step = 25` ([`[sizing]`](../../reference/configuration-more/#sizing)). With auto sizing off, a profile the advice would change ends with `runnerctl sizing apply node`, which shows `memory = "7G" -> "3.25G"` under `[profiles.node]` and asks before writing; without a terminal it needs `--yes`. The rest of the file stays as written.

## How a wait is counted

A job's wait runs from when it was queued, or when runnerd first ran if later, to its start. Time runnerd was down is left out; sizing also leaves out time spent waiting for [the host's memory](../../concepts/admission/#the-hosts-memory).

Under `measured`, only a wait for a slot alone adds slots: jobs one more slot would have started, with memory and CPU to spare (`jobs waited 300s for a slot (p95) while memory and CPU had room`).

## Auto sizing

With `auto = true` runnerd sizes each profile's `memory`, `slots` and `cpus`. At start and every 5 minutes it writes each change into the config as `runnerctl sizing apply` does, with the same budget check.

- **Memory** follows the advice, between `min_memory` and `max_memory` and under `memory_ceiling` less the mirror. A lowering moves at most `max_step` percent at a time. A raise comes only from a run that met the cap, straight to the need.
- **A lowering is taken back** at the next look when a run meets the lowered cap, or a job that succeeded before fails 3 times in a row at it. For 30 days no lowering goes under it.
- **Slots under `caps`:** one more when jobs waited a minute or more at p95, or the slots were busy half the time, while every profile's caps fit `MemoryMax`, up to `max_slots`.
- **Slots under `measured`:** as many more as waited at once, at most `max_step` percent, when jobs waited a minute or more at p95 for a slot alone. Slots cost disk, not memory: they go up only while each keeps 2G of the quarter of Docker's disk that all Docker stores share. A profile whose slots stayed 2 or more above its use for a week loses one.
- **CPUs** follow average use and throttling, not peaks. A run is held back when the CPU quota throttled it in 15% or more of its run. A job with 3 runs averaging 70% of the cap gets more `cpus`, under either policy; under `measured`, 3 held-back runs earn a raise too. Raises go up to the slice's CPUs (under `caps`, the CPU budget), while each one helps. Under `caps` they come down when `min_jobs` runs say so and none was held back.

When a bound or the budget blocks a change, `runnerctl sizing` says `No slot raise:` or `No CPU raise` and why. A raise waits `raise_interval` (1 hour) after the last change, a lowering `interval` (1 hour). A change is never written over a file you edited on disk: until runnerd runs with your edit (restart it), auto sizing changes nothing and logs why. Each change is recorded in the job history, sent as a `sizing.changed` event, and listed by `runnerctl sizing`. `runnerctl sizing undo <profile>` writes the old values back; with `auto` on, a later change can undo the undo, so bound the profile to keep it.

## Split light and heavy jobs

A runner may take any queued job of its repository whose labels it has, so it is sized for the heaviest: a 300M linter beside a 6G build waits like the build.

Serve the repository in a smaller second profile with another `[[repositories]]` entry: same `name`, its own `profile` and label. Added to [the example](../../reference/configuration/#an-example):

```toml
[host]
admission = "measured"   # the check then holds the largest runner and the mirror (9.5G) to MemoryMax

[profiles.small]
image = "ghcr.io/ayagmar/runnerd-runner:node-0.1.0"
cpus = 2
memory = "1G"
slots = 4

[[repositories]]
name = "halyard/storefront"
profile = "small"
labels = ["storefront-small"]
```

Then set `runs-on: storefront-small` on the light jobs. A job runs in the pool that has every label in its `runs-on`; `small` leaves out `self-hosted` so unmoved jobs stay on `node`. Each pool has its own slots, `warm` and `build_cache`. The check refuses two pools of a repository that share a label or profile, or whose `build_cache_writers` differ.
