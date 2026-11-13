---
title: Caches, history and notifications
description: The runnerd.toml keys for the caches, the job history, records, sizing, GitHub, the dashboard and notifications.
---

The rest of `runnerd.toml`'s keys; [Configuration](../configuration/) has the runners' and says how sizes and durations are written.

## `[caches]`

| Key | Default | |
|---|---|---|
| `mirror.enabled` | required | pull Docker Hub images through a mirror on this host |
| `mirror.memory`, `mirror.cpus` | required | the mirror's limits; `memory` counts in the budget |
| `mirror.ttl` | required | how long it keeps an image no job pulls again |
| `mirror.max_size` | `"10G"` | emptied above this when no runner runs |
| `pnpm.max_size`, `maven.max_size` | required | the shared pnpm store and Maven repository, emptied above this when no runner runs |
| `reseed_at` | `"2G"` | slot 1's layers become the profile's snapshot once they changed this much and it is idle ([Warm caches](../../guides/caches/#warm-caches)) |
| `snapshot_max` | `"10G"` | a larger snapshot starts over from the image |
| `build.enabled` | `true` | the shared [build cache](../../guides/caches/#build-cache) |
| `build.max_size` | `"10G"` | above this its least recently used entries go when no runner runs; [GitHub Actions' cache](../../guides/caches/#github-actions-cache) counts too |
| `build.branches` | `[]` | branches besides the default branch whose pushes, schedules and manual runs write to it |
| `build.directory` | `$XDG_CACHE_HOME/runnerd/build-cache` | its entries |
| `build.actions_cache` | `true` | it also serves GitHub Actions' cache in place of GitHub's |

## `[history]`

| Key | Default | |
|---|---|---|
| `database` | `$XDG_STATE_HOME/runnerd/runnerd.db` | the [job history](../../concepts/terms/#job-history)'s file |
| `samples` | `"7d"` | how long a job's CPU and memory samples are kept; totals and peaks outlive them |
| `logs` | `"30d"` | how long a finished job's log is kept |
| `events` | `"30d"` | how long events are kept for `Last-Event-ID` resumes |
| `jobs` | `"90d"` | how long a finished job is kept |
| `log_max` | `"50M"` | the most log kept per job |

## `[records]`

| Key | Default | |
|---|---|---|
| `max_total` | `"20G"` | the disk all records may take; `runnerd doctor` warns at 80% |
| `max_count` | `10` | how many are kept |
| `max_age` | `"7d"` | how long one is kept |
| `min_free` | `"10G"` | the job gets no record if it would leave Docker less free disk than this |

The oldest records go first.

## `[sizing]`

| Key | Default | |
|---|---|---|
| `margin` | `15` | percent added to a sampled peak in [advice](../../guides/sizing/) and predictions |
| `min_jobs` | `10` | the fewest runs before a peak becomes advice or a prediction |
| `auto` | `false` | runnerd sizes each profile's `memory`, `slots` and `cpus` itself (`runnerd init` writes `true`) |
| `interval` | `"1h"` | the least time between a change to a profile and one that lowers its `memory` or `cpus` |
| `raise_interval` | `"1h"` | the same for a change that only raises; at most `interval` |
| `max_step` | `50` | the most percent one change lowers `memory` or moves `cpus` |

## `[github]`

| Key | Default | |
|---|---|---|
| `job_actions` | `true` | clients may [cancel a job's run and re-run jobs](../../guides/cancel-rerun/) on GitHub with runnerd's token, each confirmed before it is sent; `false` turns both off |

## `[web]`

| Key | Default | |
|---|---|---|
| `origins` | `[]` | proxy addresses whose dashboard pages may write ([On your phone](../../guides/remote-dashboard/)): `https://host:port`, or `http` for loopback only |
| `open` | unset | the one of `origins` that `runnerctl web` opens |

## `[notify]`

[Notifications](../../guides/notifications/) go nowhere without targets.

| Key | Default | |
|---|---|---|
| `digest_at` | `"09:00"` | host-local `HH:MM` when targets that list `digest` get it |
| `targets` | `[]` | each a `[[notify.targets]]` table, below |

### `[[notify.targets]]`

| Key | Default | |
|---|---|---|
| `name` | required | what logs and `runnerctl notify test` call it |
| `kind` | required | `"ntfy"`, `"discord"` or `"webhook"` |
| `url` | required | the topic or webhook URL; a redirect counts as a failed send |
| `headers` | `{}` | sent with each post, such as `{ Authorization = "Bearer tk_…" }` |
| `events` | `["failed", "lost", "runner", "insights"]` | what it is sent; add `"digest"` |

`url` and header values are secrets: the dashboard, `runnerctl config show` and `GET /v1/config` show them as `"<secret>"`, and saving that text keeps the values. Write a new value to change one, and all values again after renaming a target. The file is mode 0600; `runnerd doctor` warns otherwise.
