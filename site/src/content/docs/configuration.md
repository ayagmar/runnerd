---
title: Configuration
description: The runnerd.toml file, runner profiles, and the memory budget check.
---

One file, `~/.config/runnerd/runnerd.toml`: the repositories to serve, the profile each one runs on, the shared caches, and the systemd slice whose `MemoryMax` is the budget. `runnerd init` writes a starter one.

## An example

Two repositories with two runners each, in a slice with `MemoryMax=33G`. [`examples/runnerd.toml`](https://github.com/ayagmar/runnerd/blob/main/examples/runnerd.toml) in the repository is another complete one.

```toml
# 2 × 9G + 2 × 7G + 512M for the mirror = 32.5G.

[host]
slice = "runnerd.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = true, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }
# Slot 1's layers become a profile's snapshot once they changed this much and the profile is idle.
reseed_at = "2G"

[profiles.jvm]
image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.0.6"
cpus = 8
memory = "9G"
slots = 2

[profiles.node]
image = "ghcr.io/ayagmar/runnerd-runner:node-0.0.6"
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

| Key | Means |
|---|---|
| `image` | the runner image the profile runs; see [below](#runner-images) |
| `cpus`, `memory` | each runner's caps |
| `slots` | how many runners of the profile may run at once |
| `record_failures` | keep each failed job's runner as a [record](#records-of-failed-jobs), to open a shell in; off by default |
| `labels` | what the repository's workflows ask for in `runs-on` |

Sizes take a binary unit (`K`, `M`, `G`, `T`), as Docker and systemd use them.

## Runner images

[`image/Dockerfile`](https://github.com/ayagmar/runnerd/tree/main/image) has a stage per profile on a common base: `base`, `jvm`, `node`. Each release publishes every stage, for linux/amd64. A profile sets one of:

| Key | Runs | Pulled |
|---|---|---|
| `image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.0.6"` | the published stage | before the profile's first runner |
| `image_stage = "jvm"` | `runnerd-runner:jvm`, built on this host | never |

runnerd logs the pull's progress, and `runnerd doctor` says whether each image is present or can be pulled. Keep the tag at the daemon's version: the image's scripts and the daemon change together, and `runnerd doctor` warns when they differ. To run your own changes to `image/`, build the stage from a clone and set `image_stage`:

```sh
docker build --target jvm -t runnerd-runner:jvm image
```

Rebuilds install the same versions: Ubuntu packages come from one snapshot date, and every download is checked against its SHA-256 ([ADR 0023](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0023-runner-image-pinned-to-a-snapshot-date.md)). [Install](../install/#verify-a-release) shows how to check where a published image came from.

## Records of failed jobs

A profile with `record_failures = true` keeps the runner of each failed job as a Docker image, so `runnerctl shell <job>` can open a shell where the job failed ([Running](../running/#records-of-failed-jobs)). `[records]` bounds them; these are the defaults:

```toml
[profiles.jvm]
image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.0.6"
cpus = 8
memory = "9G"
slots = 2
record_failures = true

[records]
max_total = "20G"   # all records together
max_count = 10
max_age = "7d"
min_free = "10G"    # left free on Docker's disk after a new record
```

A new record pushes out the oldest past `max_count` or `max_total`; without room under `min_free`, the job gets no record, and its page says why. `runnerd doctor` warns once the records take 80% of `max_total`.

## Sizing

runnerd keeps each job's sampled memory and CPU peaks. `runnerctl sizing`, the dashboard's Sizing page and the TUI turn the last 30 days of them into advice for each profile's `memory` and `slots`:

- The need is the largest peak plus a margin, rounded up to 256M. Samples are 2 s apart, so a shorter spike goes unseen. The cap kills a job that goes over it, so the need follows the heaviest run, not a percentile: a profile whose builds fit in 2G but whose end-to-end job takes 6G keeps room for the 6G. The percentiles are shown to say how rare that run is.
- Below `min_jobs` jobs there is no advice, only the peaks.
- A peak within 5% of its cap counts as at the cap: the job may have been held back or killed there and need more than it shows. A profile with such a run is never lowered, even when the run was at a smaller cap it had before.
- When the need is a tenth or more under `memory`, the advice is to lower it. If jobs waited a minute or more for a slot (p95), or the slots were busy half the time, it also adds as many slots as the profile's memory now holds.
- When the need is over `memory`, the advice is to raise it. Slots go down only if `MemoryMax` cannot hold them. When it cannot hold even one, the advice is to grow the slice first, and there is nothing to apply.

`[sizing]` changes the rules; these are the defaults:

```toml
[sizing]
margin = 15     # percent added to a sampled peak
min_jobs = 10   # runs before peaks become advice
```

What sizing cannot see:

- A job killed for memory between two samples. The history has no out-of-memory signal: a job that idles at 3G and is killed within one 2 s interval shows a 3G peak, and its failure counts as an ordinary run. Failed jobs count like passing ones. Only cancelled jobs are left out, since a cancel stops a job before its peak.
- Time the daemon was not running. Jobs queued while it was down show long waits, which can make the advice add slots.

Advice is only written when you apply it: `runnerctl sizing apply <profile>`, `a` in the TUI, or the Sizing page's button, which opens the change in the config editor. Each goes through the same check as any config, so a change `MemoryMax` cannot hold is refused. Lowering never asks for more than the profile's slots take now, and raising takes only what `MemoryMax` has spare, so two raises applied one after the other can be refused. `runnerd doctor` warns when a profile's jobs need more than its `memory`. Run without a daemon, it reads the history database read-only and does not migrate it. SQLite still leaves the `-wal` and `-shm` files a WAL database needs next to it when it may write there, so run it as the user who runs the daemon.

## The budget check

`runnerd check-config [file]`, by default on `~/.config/runnerd/runnerd.toml`, reads the slice's `MemoryMax` from systemd, or takes `--memory-max`, and fails when `slots × memory` over all profiles plus the mirror does not fit:

```text
$ runnerd check-config runnerd.toml
runnerd.toml: valid, memory caps sum to 32.5G
$ runnerd check-config runnerd.toml --memory-max 24G
error: slots × memory over all profiles plus the mirror is 32.5G, the slice runnerd.slice's MemoryMax is 24G
```

A slice without a unit file has no `MemoryMax`; the check then passes with a warning, since nothing would hold the runners to their caps.

## Isolation

`isolation = "sysbox"` is the default: each runner container runs its own Docker Engine, unprivileged.

`isolation = "unsafe-dind"` runs privileged containers, so a job can take over the host. It is refused unless `[host]` also sets `allow_unsafe = true`:

```text
$ runnerd check-config runnerd.toml
error: isolation "unsafe-dind" runs runner containers privileged, so a job can take over the host; set allow_unsafe = true under [host] to use it anyway
```

Never serve public repositories from self-hosted runners: anyone who can open a pull request could run code on your machine.
