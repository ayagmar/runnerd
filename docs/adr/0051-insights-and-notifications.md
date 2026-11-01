# 0051. Insights from the history, and notifications the daemon sends

Status: **Accepted** (2026-10-24), amended by [0064](0064-each-notify-kind-is-a-sender.md). Builds on [0034](0034-desktop-framework.md).

## Context

The history holds every job's queued, start and finish times, conclusion, memory peak and cap, run ID and attempt,
workflow and branch. Nobody reads it daily: a job that got a minute slower last Tuesday, a test that fails one run in
ten and passes on a re-run, or a job creeping toward its memory cap goes unseen until it hurts. And a failed job is
seen only by whoever opens GitHub or the dashboard.

## Decision

### Insights

`runnerd_core::insights` computes insights from the jobs of a window (default 14 days, at most 90), at a given time.
It is pure, like sizing: the daemon reads the history and the config and passes them in. A job is told apart by
repository, workflow and job name. No new data is collected.

- **Slower**: a job's successful runs on `main` or `master` when it has 10 of them, else on its busiest branch,
  oldest first. The last 5 are recent, the ones before
  are the baseline, 5 at least. The job is slower when the median of the recent runs is 20% and 30 seconds or more
  above the baseline's median and every recent run is slower than the baseline's median: a job that flips between
  two speeds, such as build cache hits and misses, moves its median with a few more slow runs in a row without
  getting slower (amended 2026-10-24, after a Gradle job read as slower on cache misses alone). A run is slow when
  it is above that threshold too. "Since" is the first slow run
  after which no two runs in a row were fast; a job whose last two runs were fast is not slower, whatever the median.
  One slow run never moves a median of five, so a one-off is not reported. Once more than half the baseline is slow,
  the slower time is the new normal and the insight goes; with a run or two a day that is about a week.
  The history does not know the default branch, and asking GitHub for it is a request per repository for a
  heuristic that is right anyway: `main` or `master` first, so on a repository whose CI also runs on pull requests a
  busy pull request that adds tests is not a regression; else the branch with the most runs of the job in the
  window. Matrix legs that share one job name are pooled; matrix jobs are usually named with their values.
- **Flaky**: a run in which an attempt of the job failed or timed out and a later attempt of the same run succeeded.
  A re-run builds the same commit, so the same run is the same code. One such run in the window is a notice, as it
  may be chance; two or more are a warning. Its rate is flaky runs over the job's runs. A job that failed and failed again on a re-run is broken, not flaky; a lost job
  that passed on a re-run is the host's fault, not the test's.
- **Near its cap**: the 95th percentile of a job's memory peaks over its last 10 finished runs (5 at least) is 85%
  of its profile's `memory` or more. It says what auto sizing does about it: raises the profile, cannot (the profile
  is at its `max_memory`), or nothing (auto sizing is off). It is a warning only when the owner has something to do
  and it is no single spike: auto sizing cannot raise it or is off, and 2 or more of those runs peaked at 85% or
  more. Otherwise it is a notice: with 5 to 9 runs the 95th percentile is the highest peak.
- **Waits**: per repository, the wait from queued to started of the jobs that started in the last 7 days against the
  7 before, p50 and p95, with the jobs that found a warm runner and those that waited for a cold start counted apart
  ([0050](0050-warm-runners.md)). These are a table, not insights; the 95th percentile rising by half and a minute or
  more is also an insight.

Each insight has a key that stays the same while it holds (`slower:<repository>/<workflow>/<job>`), a severity
(`warning`, or `notice` for waits and for the cases above that need no action), a sentence in plain words, the job's runs to open on GitHub and in the history,
and for a slower job the first slow run.

`GET /v1/insights?days=` answers the insights and the wait table. `GET /v1/digest` answers the last 24 hours: jobs
finished and failed, the insights that hold now and did not 24 hours ago, and the three longest waits. "New" is
computed the same way at any time: no state.

### Notifications

The daemon sends them; clients change nothing. `[notify]` lists targets, each with a `name`, a `kind`, a `url`,
optional `headers` and the `events` it wants:

- `kind = "webhook"`: a JSON body (`api::Notification`): the kind of event, a title, a message, a link, and the jobs,
  insights or digest it is about.
- `kind = "ntfy"`: the topic URL. runnerd publishes the same title, message and link as ntfy's JSON (to the server
  with the topic in the body), with a priority and tags, so names in any language arrive intact.

Events: `failed` (a job failed or timed out), `lost` (a job's runner vanished), `runner` (a runner could not be
registered or started: the first hold of a streak), `insights` (a new `warning` insight), `digest` (once a day
at `[notify] digest_at`, local time, default `09:00`). By default a target gets `failed`, `lost`, `runner` and
`insights`; the digest is opt-in, as it is a message every day whatever happened. Discord (a kind since
[0064](0064-each-notify-kind-is-a-sender.md)), Slack and the daemon
stopping are not targets or events: a webhook covers the first two through a relay, and a stopped daemon cannot
say it stopped; systemd and the dashboard do.

A runner that cannot start was a log line only. It becomes a durable event, `runner.failed`, stored when a
repository is first held back, so clients see it too and the notifier reads it like the others. A repository that
stays broken is held again every 10 minutes at most; those holds are not announced again until a runner takes a job,
so a revoked app or a missing image is one message, not one every 10 minutes all night. A warm runner that started,
connected and waited out its time ends a streak too, so a failure days later on a quiet repository is announced.

How the notifier runs:

- It is a task of the daemon beside auto sizing, off the engine's loop. It reads the store's durable events after
  each target's cursor, and the store wakes it when one is stored. The engine never waits for it.
- Each target's cursor (the last event delivered), the insights it was told about and its last digest are stored in
  the history (`notify` table) once a delivery succeeds, so a restart drops no message. Delivery is at least once:
  a daemon stopped between sending and storing sends that message again. A target
  seen for the first time starts at the newest event and counts the insights that hold then as told: adding
  `[notify]` does not replay a month of failures. A target removed from the config is forgotten.
- A failed delivery is retried with backoff, from 5 seconds doubling to 10 minutes, with the events that came
  meanwhile folded in. Each request times out after 10 seconds. A history that cannot be read backs off the same
  way.
- A message names and carries the first 20 jobs or runners of each kind and counts the rest, so a target whose
  endpoint refuses for an afternoon while main is red holds and sends a bounded body, not one that grows with each
  event.
- A burst is one message: the notifier waits 10 seconds after the first event before it sends, then sends to a
  target at most once a minute, the digest included. Twenty jobs failing on a broken main are one message ("20 jobs failed", the first
  five named) or two, not twenty.
- An insight is told once. It is told again only after it went away for a day, so one hovering at a threshold does
  not ping every few hours.
- The digest goes at the first check after `digest_at` on a local day it has not gone yet, so moving `digest_at`
  later does not send a second; a target added today gets its first tomorrow. A digest counts for the day it was
  made, not the day it was delivered, so one retried past midnight does not stand in for the next day's. The local offset is read from
  `date +%z` at each check, so it follows the host's time zone and daylight saving without a time zone library; a
  check that cannot read it skips the digest rather than guess UTC.
- The URL and every header value are secrets: their `Debug` shows `<secret>`, no log line or error shows them
  (reqwest errors drop their URL), and a delivery error says the target's name and the HTTP status only. A config
  check that refuses a URL says which target, not what it was.
- No client holds them either. `runnerd.toml` held no secret before, and the dashboard's config editor reaches every
  signed-in browser, a phone paired through a proxy included. `GET /v1/config` answers each target's `url` and
  header values as `<secret>`, the rest of the file as written. `PUT /v1/config` and `POST /v1/config/check` put
  the stored value back in place of each `<secret>`, by target name and field, so a config edited in the browser or
  by `runnerctl` saves with its secrets. A `<secret>` with nothing to keep, as in a target renamed in the editor, is
  refused as invalid: write the value. The owner reads and edits the file itself for the values.
  The text as sent is parsed first, masks and all, and a text that does not parse is answered with that error: a
  TOML error quotes its line, and a line runnerd had filled in would quote the secret back to the browser. Once it
  parses, filling in strings where strings were cannot break it, and `validate` names a target's field, never its
  value. Comments are not masked; the site says to keep old URLs out of them.
- Known limit: a TOML syntax error quotes the line it is on. A broken line holding a URL or header in the file on
  disk shows in the error a reload logs, as any config line does.
- Nothing leaves the machine without `[notify]`.

`POST /v1/notify/test` sends a test message to every target, or one, and answers what each did;
`runnerctl notify test` calls it.

## Consequences

- One migration: the `notify` table. One event: `runner.failed`. One config section. No new dependency: the daemon
  already builds `reqwest` for GitHub.
- The dashboard has an Insights page and the TUI's overview a line saying how many insights hold;
  `runnerctl insights` prints them, one per line, for scripts.
- The thresholds are constants, not settings. They are a first cut; they change in code when real use says so.
- A target's URL lives in `runnerd.toml`, which is the owner's file (mode 0600 from `runnerd init`); the API shows
  it masked. Reading it from a file or the environment instead is left for when someone needs the config itself
  shared. An ntfy topic
  URL is the topic's only secret; on ntfy.sh, a long random topic name or an access token in `headers` keeps
  others from reading it.
- Rejected: notifications from clients (the tray of 0034 comes later and does not run on servers), ntfy's
  `X-Template` (the JSON publish carries the same with no template to maintain), per-target thresholds, and storing
  each job's commit (the run ID already names it, and the run's page links to it).
