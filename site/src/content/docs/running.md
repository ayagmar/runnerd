---
title: Running
description: Start the runnerd daemon, how it schedules runners and keeps caches warm, and its JSON API.
---

## Build the runner image

runnerd runs the image `runnerd-runner:<image_stage>`. In a checkout of the repository, `image/Dockerfile` has a `base` stage. Append a stage on it for each profile's `image_stage`; for the [example config](../configuration/), `jvm` and `node`:

```dockerfile
FROM base AS jvm

FROM base AS node
```

Then build both:

```sh
docker build --target jvm -t runnerd-runner:jvm image
docker build --target node -t runnerd-runner:node image
```

## Start and stop the daemon

```sh
runnerd serve --config runnerd.toml
```

Without `--config`, it reads `$XDG_CONFIG_HOME/runnerd/runnerd.toml`. It takes the `gh` CLI's token at start and again when GitHub rejects it.

SIGTERM, SIGINT or `runnerd stop` stops polling and waits for running jobs. A second signal, or `runnerd stop --force`, kills them.

## How it schedules

- Every `poll_interval` (`[host]`, default `15s`) it asks GitHub for each repository's queued jobs.
- A job is served when every label in its `runs-on` is in its repository's `labels`.
- For each one it registers a just-in-time runner and starts it in a free slot of the repository's profile, if the slice's `MemoryMax` still has room for the profile's `memory`. Other jobs wait in GitHub's queue.
- The runner takes one job, its container exits, and runnerd removes it. A runner with no job after 5 minutes is removed.
- A restarted runnerd adopts the runner containers still running and deletes this host's runners that have none.

## Warm caches

A slot is one concurrent job of a profile. Each job gets a new container, but the slot keeps a volume over the profile's read-only snapshot:

| Cache | Kept |
|---|---|
| the slot's Docker images, `~/.cache`, tool cache, Android SDK | per slot, between jobs |
| the profile's snapshot | replaced by slot 1's changes once they reach `reseed_at` (`[caches]`, default `2G`) and the profile is idle |
| pnpm store, Maven repository | shared by all slots, emptied above `max_size` when no job runs |
| Docker Hub mirror | runs while any runner does |

## API

JSON on `$XDG_RUNTIME_DIR/runnerd.sock`, mode 0600: only the user runnerd runs as may connect. The types are in [`crates/core/src/api.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api.rs).

| Request | Response |
|---|---|
| `GET /v1/status` | profiles and their slots, running runners, repositories with their queued jobs, the memory budget |
| `POST /v1/repositories/{owner}/{name}/pause` | `204`; the repository gets no new runners, running jobs finish |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/shutdown` with `{"force": false}` | `202` |

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/status
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" -X POST http://runnerd/v1/repositories/halyard/ledger/pause
```
