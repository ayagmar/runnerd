---
title: Insights and notifications
description: What runnerd reads from its job history without being asked, and how it reaches a phone or a webhook when a job fails.
---

runnerd reads its [job history](../running/#job-history) for what changed in your CI, and with `[notify]` it sends failures and new findings to an [ntfy](https://ntfy.sh) topic or a webhook. Nothing leaves the machine without `[notify]`.

## Insights

| Insight | When |
|---|---|
| Slower | the median of a job's last 5 successful runs on its busiest branch is 20% and 30 s or more above the median of its runs before them, and its last two runs were not both fast. It names the first slow run. One slow run among fast ones is not enough. |
| Flaky | a job failed or timed out, then passed on a re-run of the same run. A job that fails again on the re-run is broken, not flaky. A job whose runner vanished is not counted. |
| Near its cap | the 95th percentile of a job's memory peaks over its last 10 runs is 85% or more of its profile's `memory`. It says whether [auto sizing](../admission/#auto-sizing) raises the profile, cannot, or is off. |
| Waits | a repository's 95th-percentile wait this week is half again and a minute more than the week before. A notice, not a warning. |

A job is its repository, workflow and job name. The busiest branch is the one with the most runs of that job: the default branch on a repository that runs CI on pushes. A slower job stops being reported once its slower runs are most of the runs before the last five, which with a few runs a day is about a week.

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

## Notifications

Each `[[notify.targets]]` entry is a place messages go. A burst is one message: runnerd waits 10 seconds after the first event, then sends to a target at most once a minute, so twenty jobs failing on a broken main arrive as "20 jobs failed" with the first five named. A failed delivery is retried, from 5 seconds up to every 10 minutes, with what came meanwhile folded in. Where each target got to is kept in the job history, so a restart neither drops nor repeats a message. A target added later starts with the events from then on.

| Event | Sent when | Default |
|---|---|---|
| `failed` | a job failed or timed out | on |
| `lost` | a job's runner vanished before GitHub said the job completed | on |
| `runner` | a runner could not be registered or started, and the repository gets none for a while | on |
| `insights` | a new warning insight. Each one is told once, and again only after it went away for a day. | on |
| `digest` | at `digest_at`: the day's finished and failed jobs, new insights and longest waits | off |

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

The URL and header values are secrets: no log line, error or `runnerctl notify test` answer shows them, only the target's name and the HTTP status or what failed. `GET /v1/config` and the dashboard's config editor show the file to its owner, as before.
