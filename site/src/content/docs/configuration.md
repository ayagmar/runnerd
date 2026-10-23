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

Advice is only written when you apply it: `runnerctl sizing apply <profile>`, `a` in the TUI, or the Sizing page's button, which opens the change in the config editor. Each goes through the same check as any config, so a change `MemoryMax` cannot hold is refused. Lowering never asks for more than the profile's slots take now, and raising takes only what `MemoryMax` has spare, so two raises applied one after the other can be refused. `runnerd doctor` warns when a profile's jobs need more than its `memory`. Run without a daemon, it reads the history database read-only and does not migrate it. SQLite still leaves the `-wal` and `-shm` files a WAL database needs next to it when it may write there, so run it as the user who runs the daemon. Where it may not write, it reads the database file alone, and skips the sizing check while the `-wal` holds changes the file lacks. Sizing reads the slice's `MemoryMax` on each request, so one raised after `systemctl daemon-reload` counts without restarting runnerd.

## Measured admission

By default a runner starts only when its profile's `memory` fits `MemoryMax` beside the caps of everything running, and the budget check below holds `slots × memory` to `MemoryMax`. Most jobs peak far under their caps, so that memory sits reserved and unused. `admission = "measured"` under `[host]` has each runner reserve its job's predicted peak instead:

```toml
[host]
slice = "runnerd.slice"
isolation = "sysbox"
admission = "measured"

[profiles.node]
image_stage = "node"
cpus = 8
memory = "7G"   # still each container's hard limit
slots = 8       # how many run at once; 8 × 7G may exceed MemoryMax
```

- A job is predicted from its own runs (repository, workflow and job name): with `[sizing] min_jobs` of them, their p95 peak plus `[sizing] margin`, rounded up to 256M.
- With fewer, the larger of its profile's p95 and its own largest peak, each plus the margin. With too few runs of the profile as well, the cap.
- A run that peaked within 5% of its cap, or in which the kernel killed a process for memory, puts its job back on its cap for 7 days.
- A runner may take any of its repository's queued jobs, so it reserves the largest of their predictions.
- Nothing starts while the slice uses 90% of `MemoryMax` or more. Without a sample of the slice from the last few seconds, runners reserve their caps.
- The check asks only that one runner of each profile, with the mirror, fits `MemoryMax`, and that the slice has one.

The caps still stop a container that goes over them. What measured admission trades for the shorter waits is that several jobs going over their predictions at once can fill the slice, which the kernel then answers by reclaiming page cache and killing a process inside it. Each job's record says how its runner was admitted (`runnerctl job <id>`, the dashboard's job page).

Try it on your own history before turning it on. `runnerd simulate` replays the last 30 days through both policies, each job queued when it was and running as long as it ran, and changes nothing:

```text
$ runnerd simulate --memory-max 33G --slots node=8
1555 finished jobs replayed against a MemoryMax of 33G, each queued when it was and running as long as it ran.
PEAKS is every running job's peak added up at the busiest moment, with the mirror: more than the slice ever held.

POLICY    SLOTS                   WAIT P50  WAIT P95  WAIT MAX      MEAN AT ONCE    PEAKS   UNDER  HELD
caps      jvm 2, node 2                 0s    20m56s    55m35s     3m21s       4   21.45G       0     0
caps*     jvm 3, node 8                 0s    13m34s    31m55s     1m41s       4   21.85G       0     0
measured  jvm 3, node 8                 0s    10m18s    28m38s     1m07s       9   26.78G       2     0

* more slots than the caps fit in MemoryMax, which the config check refuses with admission = "caps".
UNDER: jobs that peaked above what admission reserved for them. HELD: times the high-water mark held back a job the predictions had room for.
```

Without `--memory-max` it reads the slice's from systemd, and without `--slots` it gives each profile as many slots as `MemoryMax` holds of it alone, or its configured ones if more; `--days` sets the window. A `PEAKS` over `MemoryMax` is marked `!`.

## The budget check

`runnerd check-config [file]`, by default on `~/.config/runnerd/runnerd.toml`, reads the slice's `MemoryMax` from systemd, or takes `--memory-max`, and fails when `slots × memory` over all profiles plus the mirror does not fit:

```text
$ runnerd check-config runnerd.toml
runnerd.toml: valid, memory caps sum to 32.5G
$ runnerd check-config runnerd.toml --memory-max 24G
error: slots × memory over all profiles plus the mirror is 32.5G, the slice runnerd.slice's MemoryMax is 24G
```

A slice without a unit file has no `MemoryMax`; the check then passes with a warning, since nothing would hold the runners to their caps. Under [measured admission](#measured-admission) it holds one runner of each profile, with the mirror, to `MemoryMax`, and refuses a slice without one.

## Isolation

`isolation = "sysbox"` is the default: each runner container runs its own Docker Engine, unprivileged.

`isolation = "unsafe-dind"` runs privileged containers, so a job can take over the host. It is refused unless `[host]` also sets `allow_unsafe = true`:

```text
$ runnerd check-config runnerd.toml
error: isolation "unsafe-dind" runs runner containers privileged, so a job can take over the host; set allow_unsafe = true under [host] to use it anyway
```

Never serve public repositories from self-hosted runners: anyone who can open a pull request could run code on your machine.
