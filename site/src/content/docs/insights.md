---
title: Insights and notifications
description: What runnerd reads from its job history without being asked, and how it reaches a phone or a webhook when a job fails.
---

runnerd reads its [job history](../running/#job-history) for what changed in your CI, and with `[notify]` it sends failures and new findings to an [ntfy](https://ntfy.sh) topic, a Discord channel or a webhook. Nothing leaves the machine without `[notify]`.

## Insights

| Insight | When |
|---|---|
| Slower | the median of a job's last 5 successful runs on `main` or `master`, or its busiest branch when those have too few runs, is 20% and 30 s or more above the median of its runs before them, and each of those 5 runs is slower than that earlier median. A job that flips between two speeds, such as build cache hits and misses, is not slower. It names the first slow run. One slow run among fast ones is not enough. |
| Flaky | a job failed or timed out, then passed on a re-run of the same run. Once is a notice; twice or more, a warning. A job that fails again on the re-run is broken, not flaky. A job whose runner vanished is not counted. |
| Near its cap | the 95th percentile of a job's memory peaks over its last 10 runs in the profile it last ran in is 85% or more of that profile's `memory`. It says whether [auto sizing](../admission/#auto-sizing) raises the profile, cannot, or is off. A notice when auto sizing raises it or only one run came that close; a warning otherwise. |
| Cold cache | a job's last 5 runs that could write to the [build cache](../running/#build-cache) and looked up 10 entries or more, at least 3 such runs, found 10% or less of what they looked up: the runs before them stored what they built, so their cache keys differ from run to run, such as from a timestamp or an absolute path among a task's inputs. A notice. |
| Memory pressure | the kernel throttled the downloads of 2 or more of a repository's jobs in the window because their memory was under pressure (`sock_throttled`): the host was short of memory, or a job sat at its cap. A warning. runnerd already [holds starts back](../admission/#the-hosts-memory) while the host is short; this says it still happened. |
| Waits | a repository's 95th-percentile wait this week is half again and a minute more than the week before. A notice, not a warning. |

A job is its repository, workflow and job name. Warnings are what notifications send; notices only show. A slower job stops being reported once its slower runs are most of the runs before the last five, which with a few runs a day is about a week.

```sh
runnerctl insights
runnerctl insights --days 30
runnerctl insights --digest
```

```console wrap=false
$ runnerctl insights
warning slower    halyard/ledger: CI / test on main takes 6m 10s now, up from 4m 40s.
warning near-cap  halyard/ledger: CI / integration peaks at 8.1G, 90% of jvm's 9G. Auto sizing is off: raise jvm's memory, or turn it on.
warning flaky     halyard/storefront: CI / e2e failed and then passed on a re-run in 2 of its 31 runs.
notice  waits     halyard/storefront: 1 job in 20 waits 4m 10s or more for a runner this week, up from 1m 30s the week before.

REPOSITORY          JOBS  WAIT P50     P95  WARM P50  COLD P50  P95 WEEK BEFORE
halyard/ledger       184        3s     41s        2s       24s              44s
halyard/storefront   212        9s  4m 10s        2s       47s           1m 30s
```

The dashboard's Insights page shows the same as cards, with links to each run in the history and on GitHub. `runnerctl tui` counts the warnings on its health line.

## What the minutes would have cost

`runnerctl insights` opens with what this machine's job minutes would have cost on GitHub-hosted runners, this month (UTC) and over the last 30 days. The dashboard's overview, `runnerctl tui` and the daily digest show the month's line.

```console wrap=false
This month: 2,184 job-minutes here, $13.10 at GitHub's 2-core Linux rate, before your plan's free minutes (14 jobs not priced)
  2,184 minutes of 412 jobs at actions_linux (2-core Linux, $0.006 a minute): $13.10
  9 jobs free on GitHub: on a standard runner, of a public repository, Dependabot or Pages
  5 jobs not priced: lost, so how long they ran is not known
  An estimate from this machine's run times, at each profile's priced_as and each repository's visibility as they are now; on GitHub's runners the jobs would run longer or shorter
```

Each job that a runner here took and GitHub reported completed is billed as GitHub bills it: its minutes rounded up, at least one, at the per-minute rate of the runner its profile's [`priced_as`](../configuration/#profilesname) names. A profile with more CPUs than GitHub's 2-core runner may be worth pricing at `linux_4_core` or `linux_8_core`; the figure moves with that choice, for the whole history.

What it leaves out, and counts:

- Jobs GitHub runs free on a standard runner: those of public repositories, Dependabot's and Pages'. On a larger runner they are priced, as GitHub bills them.
- Jobs of a repository GitHub has not said is public or private. runnerd asks once a day.
- Lost jobs, and jobs without a start or an end: how long they ran is not known.

The figure is before your plan's included minutes. Those are shared with everything your account runs on GitHub's runners, which runnerd does not see, so it does not subtract them.

## Notifications

Each `[[notify.targets]]` entry is a place messages go. A burst is one message: runnerd waits 10 seconds after the first event, then sends to a target at most once a minute, so twenty jobs failing on a broken main arrive as "20 jobs failed" with the first five named; a webhook's body carries the first 20 of each kind. A failed delivery is retried, from 5 seconds up to every 10 minutes, with what came meanwhile folded in. Where each target got to is kept in the job history, so a restart drops no message; one stopped between sending and recording may send it again. A target added later starts with the events from then on.

| Event | Sent when | Default |
|---|---|---|
| `failed` | a job failed or timed out | on |
| `lost` | a job's runner vanished before GitHub said the job completed | on |
| `runner` | a runner could not be registered or started; once until a runner of that repository takes a job again | on |
| `insights` | a new warning insight. Each one is told once, and again only after runnerd saw it gone for a day: time the daemon was stopped does not count. | on |
| `digest` | at `digest_at`, once a day: the day's finished and failed jobs, new insights and longest waits. A target added today gets its first tomorrow. | off |

### On your phone with ntfy

1. Install the ntfy app and subscribe to a topic with a long random name, such as `halyard-ci-6f3a9c1e`. Anyone who knows the name can read it.
2. Add the target to `runnerd.toml`:

   ```toml
   [[notify.targets]]
   name = "phone"
   kind = "ntfy"
   url = "https://ntfy.sh/halyard-ci-6f3a9c1e"
   ```

3. Reload or restart runnerd, then send a test:

   ```sh
   runnerctl notify test
   ```

On a self-hosted ntfy server with access control, add its token: `headers = { Authorization = "Bearer tk_…" }`.

### In a Discord channel

1. In the channel's settings, under Integrations, create a webhook and copy its URL.
2. Add the target to `runnerd.toml`:

   ```toml
   [[notify.targets]]
   name = "chat"
   kind = "discord"
   url = "https://discord.com/api/webhooks/123/abc"
   ```

3. Reload or restart runnerd, then run `runnerctl notify test`.

Each message is one embed: the title, the message below it, a link to the job, red for failures, amber for lost jobs and runners, grey for insights and the digest. Mentions are off, so a job named `@everyone` pings no one. A title past 256 characters or a message past 4096 is cut, ending in `…`. When Discord limits the rate, runnerd retries as for any failed send. A `kind = "webhook"` target pointed at Discord fails with `HTTP 400 Bad Request`: Discord does not read runnerd's own JSON.

### A webhook

`kind = "webhook"` posts JSON to the URL: the event, a title, a message, a link, a priority, and the jobs, runners, insights or digest it is about (`api::Notification` in [`crates/core/src/api/insights.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api/insights.rs)).

```json
{
  "event": "failed",
  "title": "halyard/ledger: CI / test failed",
  "message": "halyard/ledger: CI / test failed on main",
  "url": "https://github.com/halyard/ledger/actions/runs/9412/job/120055991710",
  "priority": "high",
  "jobs": [{ "id": 120055991710, "repository": "halyard/ledger", "…": "…" }]
}
```

The URL and header values are secrets: no log line, error or `runnerctl notify test` answer shows them, only the target's name and the HTTP status or what failed. The dashboard's config editor, `runnerctl config show` and `GET /v1/config` show them as `"<secret>"`, and saving keeps them; read or change them in `runnerd.toml` itself. Comments are shown as written, so keep old URLs out of them. One exception: a TOML syntax error on a `url` or `headers` line quotes that line in the error, as it would any line.
