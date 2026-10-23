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
