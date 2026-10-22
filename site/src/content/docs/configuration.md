---
title: Configuration
description: The runnerd.toml file, runner profiles, and the memory budget check.
---

One file, `runnerd.toml`, lists the repositories to serve, the runner profile each one uses, the shared caches and the systemd slice whose limits form the budget.

## An example

This is the setup runnerd is first built for: two repositories with two runners each, inside a slice with `MemoryMax=33G`. It is [`examples/runnerd.toml`](https://github.com/ayagmar/runnerd/blob/main/examples/runnerd.toml) in the repository.

```toml
# 2 × 9G + 2 × 7G + 512M for the mirror = 32.5G.

[host]
slice = "ci.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = true, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.trellis]
image_stage = "trellis"
cpus = 8
memory = "9G"
slots = 2

[profiles.glowly]
image_stage = "glowly"
cpus = 8
memory = "7G"
slots = 2

[[repositories]]
name = "ayagmar/trellis"
profile = "trellis"
labels = ["self-hosted", "linux", "x64", "trellis"]

[[repositories]]
name = "ayagmar/glowly"
profile = "glowly"
labels = ["self-hosted", "linux", "x64", "glowly"]
```

A profile is one kind of runner: the stage of the [runner image](https://github.com/ayagmar/runnerd/tree/main/image) it runs, its CPU and memory caps, and how many runners of it may run at once (`slots`). Each repository picks a profile and the labels its workflows ask for in `runs-on`.

Sizes take a binary unit (`K`, `M`, `G`, `T`), as Docker and systemd use them.

## The budget check

```sh
runnerd check-config runnerd.toml
```

This reads the slice's `MemoryMax` from systemd and fails when `slots × memory` over all profiles, plus the Docker Hub mirror, does not fit in it:

```text
$ runnerd check-config runnerd.toml
runnerd.toml: valid, memory caps sum to 32.5G
$ runnerd check-config runnerd.toml --memory-max 24G
error: slots × memory over all profiles plus the mirror is 32.5G, the slice ci.slice's MemoryMax is 24G
```

`--memory-max` checks against a size you pass instead of asking systemd. A slice without a unit file has no `MemoryMax`; the check then passes with a warning, since nothing would hold the runners to their caps.

## Isolation

`isolation = "sysbox"` is the default: each runner container runs its own Docker Engine without being privileged.

`isolation = "unsafe-dind"` runs privileged containers, so a job can take over the host. It is refused unless the `[host]` table also sets `allow_unsafe = true`. Never serve public repositories from self-hosted runners: anyone who can open a pull request could run code on your machine.
