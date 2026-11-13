---
title: How do I hear when a job fails?
description: Send failures, lost runners and new insights to an ntfy topic, a Discord channel or a webhook.
---

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
| `insights` | a new warning insight, told again only after runnerd saw it gone for a day; `host-held` once in each 6-hour window, `host-low` once a day | on |
| `digest` | daily at `digest_at` (default `09:00`): the day's jobs, new insights, longest waits, the host's memory while CI ran | off |

A burst is one message: runnerd waits 10 seconds after the first event and sends to a target at most once a minute, so twenty failures arrive as one. A failed delivery is retried with backoff, 5 seconds to 10 minutes.

A target's URL and header values are secrets: logs, errors and `runnerctl notify test` never show them, and the config editor, `runnerctl config show` and `GET /v1/config` show `"<secret>"`. Change them in `runnerd.toml` itself.
