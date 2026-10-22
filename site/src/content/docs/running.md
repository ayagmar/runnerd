---
title: Running
description: Start the runnerd daemon, how it schedules runners and keeps caches warm, and its JSON API.
---

## Build the runner image

runnerd runs the image `runnerd-runner:<image_stage>`. Build the stages your profiles use before starting it, from a checkout of the repository:

```sh
docker build --target trellis -t runnerd-runner:trellis image
```

## Start the daemon

```sh
runnerd serve --config runnerd.toml
```

Without `--config`, it reads `$XDG_CONFIG_HOME/runnerd/runnerd.toml`. It reads the `gh` CLI's token at start (and again when GitHub rejects it), then every `poll_interval` (`[host]`, default `15s`) asks GitHub for each repository's queued jobs.

A job is served when every label in its `runs-on` is one of its repository's `labels`. For each such job runnerd registers a just-in-time runner on the repository and starts it in a free slot of the repository's profile, if the slice's `MemoryMax` still has room for the profile's `memory`. The runner takes one job, its container exits, and runnerd removes it; a runner that got no job within 5 minutes is removed. Jobs that cannot be served yet wait in GitHub's queue.

A restarted runnerd adopts the runner containers still running and deletes this host's runners that have no container.

## Stop it

SIGTERM or SIGINT, or `runnerd stop`, stops polling and waits for running jobs. A second signal, or `runnerd stop --force`, kills them.

## Warm caches

A slot is one concurrent job of a profile. Containers are new for every job, but each slot keeps a volume with its Docker Engine's images and its own changes to `~/.cache`, the tool cache and the Android SDK, over the profile's read-only snapshot.

When a profile has no job running and its slot 1 changed those by `reseed_at` (`[caches]`, default `2G`), slot 1's changes become the new snapshot. The pnpm store and the Maven repository are shared by all slots and emptied above their `max_size` when no job runs. The Docker Hub mirror runs while any runner does.

## API

runnerd serves JSON on `$XDG_RUNTIME_DIR/runnerd.sock`. The socket has mode 0600, so only the user runnerd runs as may connect. The types are in [`crates/core/src/api.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api.rs).

| Request | Response |
|---|---|
| `GET /v1/status` | profiles and their slots, running runners, repositories with their queued jobs, the memory budget |
| `POST /v1/repositories/{owner}/{name}/pause` | `204`; the repository gets no new runners, running jobs finish |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/shutdown` with `{"force": false}` | `202` |

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/status
```
