---
title: Running
description: Run runnerd as a user service, how it schedules runners and keeps caches warm, and its JSON API.
---

## The service

`runnerd init` writes a user unit that runs `runnerd serve`:

```ini
[Service]
Type=exec
ExecStart="/home/dana/.local/bin/runnerd" serve --config "/home/dana/.config/runnerd/runnerd.toml"
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now runnerd   # start now and at login
loginctl enable-linger "$USER"          # keep it running while you are logged out
journalctl --user -u runnerd -f         # its log
systemctl --user restart runnerd        # after editing runnerd.toml by hand
systemctl --user stop runnerd           # waits for running jobs, up to systemd's stop timeout
```

A config saved from the dashboard or `PUT /v1/config` applies without a restart. `runnerd stop` waits for running jobs too; `runnerd stop --force`, or a second signal, kills them.

Runner containers live in Docker, not in the unit: a restarted runnerd adopts the ones still running, and keeps the repositories it had paused paused.

## How it schedules

- It reads the `gh` CLI's token at start, and again when GitHub rejects it.
- Every `poll_interval` (`[host]`, default `15s`) it asks GitHub for each repository's queued jobs.
- A job is served when every label in its `runs-on` is in its repository's `labels`.
- For each one it registers a just-in-time runner and starts it in a free slot of the repository's profile, if the slice's `MemoryMax` still has room for the profile's `memory`. Other jobs wait in GitHub's queue.
- The runner takes one job, its container exits, and runnerd removes it. A runner with no job after 5 minutes is removed.
- A restarted runnerd adopts the runner containers still running and deletes this host's runners that have none.
- Before a profile's first runner it pulls the profile's `image`, logging the pull's progress.

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
| `POST /v1/repositories/{owner}/{name}/pause?force=` | no new runners; `409` naming the running job, unless `force=true` cancels its run |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/repositories/pause?force=`, `POST /v1/repositories/resume` | the same for every repository; `409` naming the running jobs pauses none |
| `POST /v1/profiles/{name}/reseed` | `202`; reseeds once the profile's slots are free |
| `POST /v1/caches/{name}/clean` | `204`; `409` naming the jobs that use it |
| `GET /v1/config`, `PUT /v1/config`, `POST /v1/config/check` | the config file; replace it and reload, or check one |
| `GET /v1/doctor` | `runnerd doctor`'s checks |
| `POST /v1/shutdown` with `{"force": false}` | `202` |

A pause lasts until a resume, across restarts.

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/status
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" -X POST http://runnerd/v1/repositories/halyard/ledger/pause
```
