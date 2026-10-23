---
title: Running
description: Run runnerd as a user service, how it schedules runners and keeps caches warm, what its job history keeps, and its JSON API.
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

## Job history

runnerd records every job its runners may run in a SQLite database, `$XDG_STATE_HOME/runnerd/runnerd.db` by default. `[history]` in the config changes the path and how long each part is kept. For each job:

| What | Kept |
|---|---|
| its timeline: queued, container started, job started, finished, container and runner registration removed | with the job |
| its steps with their start and end, as GitHub shows them | with the job |
| CPU, memory and network of its container, sampled every 2 s | samples 7 days (`history.samples`); totals and peaks with the job |
| the bytes the Docker Hub mirror served it | with the job |
| its log, as GitHub shows it with masked secrets still masked, up to 50M (`history.log_max`) | 30 days (`history.logs`) |

A finished job's record goes after 90 days (`history.jobs`), with its steps and runner. The database file does not shrink when rows go; SQLite reuses the space. runnerd also keeps the sizes of the cache volumes and the slice's CPU and memory over time.

Where the data comes from:

- **Which job a runner took**: a small script runnerd runs in the container (`docker exec`, as root) reads the job's ID from the runner's `_diag/Worker_*.log` within a second of the job starting. GitHub's list of queued jobs still shows the job for several seconds after that, and runnerd does not start a second runner for it.
- **Steps**: GitHub's jobs API, read every poll while a job runs. The runner's own logs name steps differently from GitHub (`Set up runner` for `Set up job`) and change between runner versions. The API's times have second precision.
- **The log**: the runner's `_diag/pages`, which the same script follows. The runner writes the log there in 4 KiB blocks, so lines arrive a block at a time, and a quiet step's lines arrive when it ends.
- **CPU and memory**: the container's cgroup files, memory without page cache as `docker stats` counts it; network from `/proc/<pid>/net/dev`.

## API

JSON on `$XDG_RUNTIME_DIR/runnerd.sock`, mode 0600: only the user runnerd runs as may connect. The types are in [`crates/core/src/api.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api.rs) and its modules. Times are seconds since the Unix epoch, sizes bytes, CPU millicores.

| Request | Response |
|---|---|
| `GET /v1/status` | profiles and their slots, running runners, repositories with their queued jobs, the memory budget with the mirror's and each seed container's share |
| `POST /v1/repositories/{owner}/{name}/pause?force=` | no new runners; `409` naming the running job, unless `force=true` cancels its run |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/repositories/pause?force=`, `POST /v1/repositories/resume` | the same for every configured repository; while any runs a job, `409` naming the jobs, and none is paused |
| `POST /v1/profiles/{name}/reseed` | `202`; the profile starts no runner until its slots are free and slot 1 became its snapshot; `409` while it reseeds |
| `POST /v1/caches/{name}/clean` | `204`; removes a slot's volume or a shared cache; `409` naming the jobs that use it |
| `GET /v1/config` | the config file's path and text |
| `POST /v1/config/check` with TOML | what `runnerd check-config` says about it |
| `PUT /v1/config` with TOML | checks it, writes it in place of the file and runs with it; invalid (`422`), not writable (`500`) or runnerd stopping (`503`) leaves the file and what runs as they were |
| `GET /v1/doctor` | `runnerd doctor`'s checks, for the config runnerd runs with |
| `POST /v1/shutdown` with `{"force": false}` | `202` |
| `GET /v1/jobs?repository=&state=&conclusion=&since=&until=&before=&limit=` | a page of jobs, newest first; `next` is the `before` of the next page |
| `GET /v1/jobs/{id}` | a job with its timeline, steps, metrics and log size |
| `GET /v1/jobs/{id}/samples` | its CPU, memory and network over time |
| `GET /v1/jobs/{id}/logs?from=&follow=` | its log as text; with `follow=true`, open until the job's record is final |
| `GET /v1/host/samples?since=` | the slice's CPU and memory, the last 24 hours at most |
| `GET /v1/stats?repository=&since=&until=&bucket=day` | finished jobs per repository and day (or `hour`), 744 buckets back at most: counts, queue and run time, CPU, memory, mirror bytes |
| `GET /v1/caches`, `GET /v1/caches/history?since=` | cache volume sizes and their limits, now and over time |
| `GET /v1/events?job=&logs=` | server-sent events, below |

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/status
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" -X POST http://runnerd/v1/repositories/halyard/ledger/pause
curl -N --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/events
```

### Events

| Event | Sent when |
|---|---|
| `job.queued`, `job.started`, `job.step`, `job.finished` | a job moves on |
| `slot.changed` | a runner comes or goes in a slot |
| `profile.changed` | a reload adds or changes a profile, or it starts or stops reseeding |
| `profile.removed` | a reload removed it and its last runner is gone |
| `repository.changed` | paused or resumed, its queue changed, or a reload added or changed it |
| `repository.removed` | a reload removed it |
| `budget.changed`, `cache.reseeded` | the budget changed; a profile has a new snapshot |
| `job.sample`, `host.sample`, `log.line` | live: a sample, or a log line with `logs=true` |

Every event but the live ones has an ID: send the last one seen as `Last-Event-ID` and the stream resumes after it, across daemon restarts. Live events are not stored; a client that missed some reads the job's samples or log.

`slot.changed`, `profile.changed`, `budget.changed` and `repository.changed` carry the state after a change, not each transition: a pause and a resume that land before the daemon publishes its status send nothing. Applying `profile.*` and `slot.changed` to a status's profiles, and `repository.*` to its repositories, keeps them equal to what `/v1/status` returns.

### Pausing

A forced pause cancels the run of each of the repository's running jobs. GitHub cancels runs, not single jobs, so the run's other jobs are cancelled too. The runner reports the job cancelled, which takes under a minute; one still running 90 s later is removed, and its registration is deleted once GitHub lets go of it.

A pause lasts until a resume, across restarts: the job history records the paused repositories, and a restarted daemon starts no runner for them. If the history cannot record a pause or resume, the change holds until the daemon restarts, and the request fails with `500` saying so. A reload that removes a paused repository forgets its pause; added back, it starts active.

### Reloading

A new config takes effect without a restart, and running jobs keep running. A removed profile, or slots past a lowered `slots`, stay until their runners finish, and no runner starts in a profile while it runs as many as its `slots`. A runner still waiting for a job of a removed repository, or of one moved to another profile, is removed. Runners started afterwards get the new limits, image and labels. `history.database` changes only at the next start.
