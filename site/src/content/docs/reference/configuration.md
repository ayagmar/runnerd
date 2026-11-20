---
title: Configuration
description: Every key of runnerd.toml with its default, a complete example, and what applies without a restart.
---

`~/.config/runnerd/runnerd.toml` names the repositories to serve, the profile each runs on, the shared caches and the systemd slice whose `MemoryMax` is the budget. `runnerd init` writes one sized for the host; `runnerd check-config` checks one against the budget ([The budget check](../../concepts/terms/#slice-and-budget)).

## An example

Two runners each for two repositories in a slice with `MemoryMax=33G`; 2 × 9G + 2 × 7G + 512M for the mirror is 32.5G ([`examples/runnerd.toml`](https://github.com/ayagmar/runnerd/blob/main/examples/runnerd.toml) is a complete one):

```toml
[host]
slice = "runnerd.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = true, memory = "512M", cpus = 1, ttl = "168h", max_size = "10G" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.jvm]
image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.1.4"
cpus = 8
memory = "9G"
slots = 2

[profiles.node]
image = "ghcr.io/ayagmar/runnerd-runner:node-0.1.4"
cpus = 8
memory = "7G"
slots = 2

[[repositories]]
name = "halyard/ledger"
profile = "jvm"
labels = ["self-hosted", "linux", "x64", "jvm"]

[[repositories]]
name = "halyard/storefront"
profile = "node"
labels = ["self-hosted", "linux", "x64", "node"]
```

## Every key

A key marked required must be set. Sizes take `K`, `M`, `G` or `T`, durations a number and `s`, `m`, `h` or `d`. The check refuses 0 for a profile's `cpus`, `memory` and `slots`, an enabled mirror's `memory` and `cpus`, an enabled build cache's `max_size`, `poll_interval`, `records.max_total` and `max_count`, and the `[sizing]` counts and intervals.

### `[host]`

| Key | Default | |
|---|---|---|
| `slice` | required | the systemd slice runners run in; its `MemoryMax` is the [budget](../../concepts/terms/#slice-and-budget) |
| `isolation` | required | `"sysbox"`, or `"unsafe-dind"` with `allow_unsafe = true` ([Isolation](../../security/#isolation)) |
| `allow_unsafe` | `false` | lets `isolation = "unsafe-dind"` pass the check |
| `poll_interval` | `"15s"` | how often GitHub is asked for an idle repository's queued jobs ([How often GitHub is asked](../../concepts/how-it-runs/#how-often-github-is-asked)) |
| `admission` | `"measured"` | a runner reserves its job's predicted peak, its profile's `memory` until the job has a history; `"caps"` always reserves `memory`. `"measured"` needs the slice's `MemoryMax` ([Admission](../../concepts/admission/#measured-admission)) |
| `pooled_count` | `true` | under `measured`, `MemoryMax` and the host's limit count what runners use and keep room for their largest growths, not every reservation ([The growth count](../../concepts/growth-and-priority/#the-growth-count)) |
| `skip_syncs` | `true` | runners of an image with the filter are asked to answer `fsync` and the other sync calls without syncing, also on a descriptor the kernel would refuse, and to mount their layers so that stopping the runner syncs nothing; after a host crash, not an orderly reboot, runnerd empties the slots' volumes and the pnpm and Maven stores. `false` keeps the syncs, as does an image without the filter. A reload applies it from the next runner on ([What a crash can leave](../../guides/caches/#what-a-crash-can-leave)) |
| `cpu_overcommit` | unset | a factor above 0, at most 16; a runner must also fit the slice's CPUs times it ([CPU admission](../../concepts/cpu/)) |
| `memory_ceiling` | unset | for a VM whose memory grows on demand, the most it grows to ([A host whose memory grows](../../concepts/admission/#a-host-whose-memory-grows)) |

### `[profiles.<name>]`

| Key | Default | |
|---|---|---|
| `image`, `image_stage` | required, one of them | the published image to pull, or the stage of `image/Dockerfile` built on the host ([Runner images](../../guides/images/)) |
| `cpus`, `memory` | required | each runner's CPU quota and memory limit ([Sharing the CPUs](../../concepts/cpu/#sharing-the-cpus)) |
| `slots` | required | the most runners at once |
| `record_failures` | `false` | keep failed jobs as [records](../../guides/records/) |
| `min_memory`, `max_memory` | unset | bounds for sizing advice and [auto sizing](../../guides/sizing/#auto-sizing); unset, what its jobs need and `MemoryMax` holds |
| `max_slots` | unset | the most `slots` auto sizing raises to; unset, as many as `MemoryMax` holds, or under `measured` as leave each slot 2G of disk |
| `priced_as` | `actions_linux`, `actions_linux_arm` on aarch64 | the GitHub-hosted runner its jobs are [priced at](../../guides/insights/#what-the-minutes-would-have-cost), by billing name, such as `linux_4_core` |

### `[[repositories]]`

| Key | Default | |
|---|---|---|
| `name` | required | `owner/name`; repeat the entry for [several profiles](../../guides/sizing/#split-light-and-heavy-jobs) |
| `profile` | required | the profile its jobs run on |
| `labels` | required | at least one; a job runs here when all its `runs-on` labels are listed |
| `warm` | `1` | [warm runners](../../concepts/terms/#warm-runner) kept while active; `0` turns them off |
| `build_cache` | `true` | its runners get the build cache |
| `build_cache_writers` | `"default-branch"` | whose jobs write to it: `"pull-requests"` adds [people's pull requests from its own branches](../../guides/caches/#build-cache) |
| `rerun_runner_faults` | `true` | [re-run a run's failed jobs once](../../guides/cancel-rerun/#runner-faults) when its runner, not its code, failed one |

The keys for the caches, the job history, records, sizing, GitHub, the dashboard and notifications are on [Caches, history and notifications](../configuration-more/).

## Reloading

A config saved through the dashboard or `PUT /v1/config` applies without a restart, and running jobs keep running; after a hand edit, restart the service. A removed profile, or slots past a lowered `slots`, stay until their runners finish. A waiting runner, warm ones included, is removed when its repository is removed or moved, or its `labels` or the profile's `image`, `memory` or `cpus` changed, by hand or by auto sizing; a new `image` waits until Docker has it. `[web]`, `history.database`, `caches.build.directory` and `caches.actions.directory` change only at the next start, and a change to `host.slice` is refused.
