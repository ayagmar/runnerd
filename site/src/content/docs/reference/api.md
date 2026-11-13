---
title: HTTP API
description: The JSON API every client uses, over the daemon's Unix socket, and its server-sent events.
---

JSON over `$XDG_RUNTIME_DIR/runnerd.sock` (`curl --unix-socket`), mode 0600: only the user runnerd runs as may connect. Types: [`crates/core/src/api.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api.rs). Times are Unix seconds, sizes bytes, CPU millicores.

## Routes

Jobs, their logs, the timeline and job actions are on [Jobs API](../api-jobs/).

### `GET /v1/status`

Profiles, slots, runners, repositories with their queued jobs, the budgets.

### `POST /v1/repositories/{owner}/{name}/pause?force=`, `.../resume`

`409` naming the running job, unless `force=true` cancels its run; `204`. Without `{owner}/{name}`, for every repository.

### `POST /v1/profiles/{name}/reseed`

`202`; `409` while it reseeds or a record shell is open in it.

### `POST /v1/caches/{name}/clean`

`204`; `409` naming the jobs that use the cache.

### `GET /v1/config`, `POST /v1/config/check`, `GET /v1/doctor`

The file's text and revision, secrets as `"<secret>"`; `check-config` on a TOML body; `runnerd doctor`'s checks from the daemon's last run, at most a minute old, with whether they are current; it waits up to 20 s for a new run, then answers the last one as it stands; `503` before the first run finishes or when the last failed.

### `GET /v1/attention`

What needs attention now, most urgent first, each item worded with what clears it; `revision` changes with the content and counts from 1 in each daemon `instance`.

### `PUT /v1/config?revision=` with TOML

Checks, writes and applies it; `422` invalid, `500` not writable, `503` stopping, all leaving everything as it was. A `revision` older than the file (an edit, or [auto sizing](../../guides/sizing/#auto-sizing)) answers `409`.

### `POST /v1/shutdown` with `{"force": false}`

`202`; `true` kills running jobs.

### `POST /v1/notify/test?target=`

A test message to every `[notify]` target or the one named; `404` without targets.

### `GET /v1/records`, `DELETE /v1/records/{job}`, `DELETE /v1/records?all=true`, `?older_than=2d`

The records; the ones removed (a record with a shell open stays; by job it answers `409`).

### `POST /v1/records/{job}/shell?step=&cols=&rows=` with `Connection: Upgrade`, `Upgrade: runnerd-tty`

`101`, then raw terminal bytes both ways; `409` when the budget is full.

### `GET /v1/dashboard`, `POST /v1/dashboard/sign-out`, `POST /v1/dashboard/pairings?origin=`

`[web] open`; signs every browser out; a 5-minute login link for one of `[web] origins`.

### `GET /v1/events?job=&logs=`

Server-sent events, below.

Read-only: `GET /v1/host/samples`, `/v1/stats`, `/v1/activity`, `/v1/sizing` ([advice](../../guides/sizing/)), `/v1/insights?days=` ([Insights](../../guides/insights/); 14 days by default, 90 at most), `/v1/digest`, `/v1/savings`, `/v1/caches` and `/v1/caches/history`.

A forced pause cancels the runs of the repository's running jobs, other jobs of those runs included, and removes a runner still running 90 s later, a restart in between or not. A paused repository's waiting runners, warm ones too, stop at once. A pause lasts until a resume, across restarts.

## Events

`job.queued`, `job.started`, `job.step` and `job.finished` follow a job. `slot.changed`, `budget.changed`, `profile.changed`, `profile.removed`, `repository.changed` and `repository.removed` carry the state after a change, not each transition. `cache.reseeded` marks a new snapshot, `record.created`, `record.deleted`, `record.skipped` and `record.gone` (its image removed outside runnerd) a record's fate, `sizing.changed` an [auto sizing](../../guides/sizing/#auto-sizing) write, and `runner.failed` a runner that could not be registered or started (once, until one of the repository's runners takes a job). `job.sample`, `host.sample`, `log.line` (`logs=true`) and `attention.changed`, which carries `GET /v1/attention`'s `instance` and new `revision`, are live.

Every other event has an ID. Send the last one seen as `Last-Event-ID` and the stream resumes after it, across daemon restarts.
