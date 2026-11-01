---
title: Insights and notifications
description: What runnerd reads from its job history without being asked, and how it reaches a phone or a webhook when a job fails.
---

runnerd reads its [job history](../running/#job-history) for what changed in your CI. With `[notify]` it sends failures and new findings to an [ntfy](https://ntfy.sh) topic, a Discord channel or a webhook.

## Insights

A job is its repository, workflow and job name. Notifications send warnings, not notices.

| Insight | When |
|---|---|
| `slower` | The median of a job's last 5 successful runs on `main` or `master` is 20% and 30 s or more above the median of the runs before, and each of the 5 is slower than it. |
| `flaky` | A job failed or timed out, then passed on a re-run. Once is a notice, twice a warning. |
| `near-cap` | The p95 of a job's memory peaks over its last 10 runs is 85% or more of its profile's `memory`. A notice when [auto sizing](../admission/#auto-sizing) raises it, else a warning. |
| `cold-cache` | At least 3 of a job's last 5 runs that could write to the [build cache](../running/#build-cache) looked up 10 or more entries and found 10% or less. A notice. |
| `throttled` | The kernel throttled the downloads (`sock_throttled`) of 2 or more of a repository's jobs. A warning. |
| `cpu-held` | At least 3 of a job's last 10 successful runs were throttled by the CPU quota in 15% of their run or more. A notice while auto sizing raises `cpus`, else a warning. |
| `waits` | A repository's p95 wait this week is half again and a minute more than last week's. A notice ([how a wait is counted](../admission/#how-a-wait-is-counted)). |

```console wrap=false
$ runnerctl insights
This month: 264 job-minutes here, about $1.58 at GitHub's 2-core Linux rate before your plan's free minutes
…

warning slower    halyard/ledger: CI / test on main takes 6m 10s now, up from 4m 40s.
warning near-cap  halyard/ledger: CI / integration peaks at 8.1G, 90% of jvm's 9G. Auto sizing is off: raise jvm's memory, or turn it on.
notice  waits     halyard/storefront: 1 job in 20 waits 4m 10s or more for a runner this week, up from 1m 30s the week before.
```

`--days N` sets the window (default 14, at most 90); `--digest` shows the last 24 hours.

## What the minutes would have cost

`runnerctl insights` opens with what this machine's job minutes would have cost on GitHub-hosted runners, this month (UTC) and over the last 30 days. Each job is billed as GitHub bills it: minutes rounded up, at the rate of the runner its profile's [`priced_as`](../configuration/#profilesname) names. Not counted:

- Jobs of public repositories on a standard runner, which GitHub runs free, and of repositories whose visibility is unknown.
- Jobs named `Dependabot Updates` or `pages build and deployment`, which GitHub runs free; a name is not proof, so they are neither priced nor called free.
- Lost jobs and jobs without a start or end.

The figure is before your plan's included minutes. When pruning has dropped jobs a window covers, the line says since when the history is complete.

## Notifications

Each `[[notify.targets]]` entry has a `name`, a `kind` (`ntfy`, `discord` or `webhook`), a `url`, and optional `headers` and `events`:

```toml
[[notify.targets]]
name = "phone"
kind = "ntfy"
url = "https://ntfy.sh/halyard-ci-6f3a9c1e"   # anyone who knows the topic can read it
```

Reload or restart runnerd, then `runnerctl notify test` sends every target a test message, or only `--target <name>`. On a self-hosted ntfy server with access control, add `headers = { Authorization = "Bearer tk_…" }`. A Discord `url` is the channel's webhook; each message is one embed that pings no one. A `webhook` target posts JSON (`api::Notification` in [`crates/core/src/api/insights.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api/insights.rs)); Discord rejects it.

| Event | Sent when | Default |
|---|---|---|
| `failed` | a job failed or timed out | on |
| `lost` | a job's runner vanished before the job completed | on |
| `runner` | a runner could not start | on |
| `insights` | a new warning insight, told again only after runnerd saw it gone for a day | on |
| `digest` | daily at `digest_at` (default `09:00`): the day's jobs, new insights, longest waits | off |

A burst is one message: runnerd waits 10 seconds after the first event and sends to a target at most once a minute, so twenty failures arrive as one. A failed delivery is retried with backoff, 5 seconds to 10 minutes.

A target's URL and header values are secrets: logs, errors and `runnerctl notify test` never show them, and the config editor, `runnerctl config show` and `GET /v1/config` show `"<secret>"`. Change them in `runnerd.toml` itself.
