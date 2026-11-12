# 0132. One attention list that core words, with a cached doctor and five views

Status: **Accepted** (2026-11-12). Amends [0093](0093-clients-share-views-columns-and-health-figures.md): its
list of views. No schema or config change.

## Context

What needs the owner's attention was scattered: a paused repository's jobs showed only as waiting, a repository
GitHub refused to list showed nowhere, the doctor ran only when someone asked for it, and the insights sat on a page of
their own. Each client would have had to gather these and decide what counts, which ADR 0062 forbids. `GET /v1/doctor`
also read the host on every request, with no bound on the commands it ran, so a hung `gh` or Docker held the request.

The views had grown to seven, in an order no client kept: Overview, History, Records, Sizing, Insights, Caches,
Config.

## Decision

### The list

`GET /v1/attention` answers an `Attention`: a `revision`, up to `ATTENTION_MAX` (50) items and the count `omitted`
beyond them. `api::attention_items(status, doctor, insights, now)` decides and words every item; the daemon, the
fixtures and the dashboard's demo call it, and no client builds one. An item has a `key` that stays while its condition
holds, a `kind`, a `tone`, a `title`, a `subject` (a job, a repository, a doctor check, an insight, the host or
GitHub), the time its evidence began or was taken when one is known, and a `fix` as data: a command, a config key, a
view to open, or words.

An item is listed only on evidence that its condition holds now. Nothing is inferred from history or occupancy:

- **Cannot start**: queued jobs whose `Waiting::cannot_start()` holds, that is, their repository is paused or no pool
  serves their labels. One item per repository and reason: "runnerd cannot start 2 queued jobs of halyard/ledger with
  the current configuration: …", with `runnerctl resume` or the config key `repositories.labels` as the fix. Backoff
  is transient and is not listed; a reason this daemon cannot read is not either.
- **Host memory**: the `host-held` insight says a job waited 30 minutes or more for the host's memory, and a queued job
  waits for it now. The insight is then not listed again.
- **Doctor**: each failing (`bad`) or warning (`warn`) check of a run for the config the daemon runs with. A doctor
  that has not finished, failed or is stale gives one `doctor-unavailable` item in its place, never an empty list.
- **Insights**: those of `warning` severity, except the host's own (above).
- **GitHub**: `RepositoryStatus.listing` says why a repository's queued jobs cannot be listed, as a class and an HTTP
  status, never GitHub's words. A missing or refused token is one `github-auth` item for every repository; any other
  failure one `github-listing` item per repository. The engine sets it on a failed listing, keeps when the failures
  began, and clears it on an answer or the repository's removal; a restart forgets it. A rate limit used up is one
  `rate-limited` item, from `Status::github_limited_until`.
- **Sizing and caps** give no item: no source says now that a job is blocked by its profile's size. The sizing page
  says what it advises; an item would repeat it without new evidence.

Items are ordered by tone (bad, warn, neutral, quiet, good), then those with an evidence time before those without,
newest first, then by key. Nothing tracks when a condition began beyond what its source already keeps.

### Liveness

The daemon's attention worker owns the snapshot. It looks again when the status, the doctor's run or the config
changes, on each durable event, and every 30 seconds, so a condition that comes or goes with time alone shows within
that bound. It reads the insights at most once a minute, and at once after the config is replaced. The revision moves
only when the content does, also when one item replaces another at the same count, and each move is sent on
`/v1/events` as the live event `attention.changed {revision}`. A client refetches on that event, or polls.

The revision starts at the daemon's start time in milliseconds, so a restart never answers a revision a client already
holds. Nothing of the snapshot survives a restart: the first one is computed from the status with the doctor not yet
run, so it is never an all clear.

A config replaced now reaches each of its readers on its own: a `watch` of the config's revision replaces the single
`Notify`, and the notifier, the doctor worker and the attention worker each hold a receiver.

### The doctor's cache

One worker runs the doctor's checks, one run at a time on a blocking thread: at the start, after each config replaced,
when a request finds no run of the current config under a minute old, and every 10 minutes. Its last run is a sealed
value: not run, or done for a config revision at a time with its checks or its error. Reloads that come during a run
make one run after it, and a run whose config was replaced while it ran is dropped. Each command (`systemctl`, `gh`)
is killed past 20 seconds and each Docker request is abandoned past 20 seconds; a run past 2 minutes is reported failed
and awaited before the next starts. A run older than 15 minutes, or of a config no longer running, is stale.

`GET /v1/doctor` reads the same cache, with `Doctor::state` (`not-run`, `ready`, `stale`, `failed`): a request that
finds no recent run asks the worker for one and waits for it, up to its deadline. While no run has finished, or when
the last failed, it answers 503 with the state's line. `runnerd doctor` on the command line still reads the host
itself. Nothing of the cache survives a restart, and a stop leaves a run going to end by its own deadlines.

### Five views

`api::View` is Overview, Jobs, Insights, Capacity and Settings. Each lists its `api::Section`s: Jobs holds History and
Records, Capacity holds Sizing and Caches, Settings holds Config. Every page of before is reachable in its view: the
dashboard's navigation names the five views and, on a view of several sections, links each; the TUI's digit opens a
view's first section it has and the same digit again the next. `View` and `Tone` are wire data now, for `Fix::Open`
and `AttentionItem::tone`.

### Two versions

- An older client and this daemon: `/v1/status`, `/v1/doctor` and the events read as before; `listing` and `state`
  are new fields left out when empty, and `attention.changed` is an event type the older client skips. `/v1/doctor`
  answering 503 before the first run is new; an older client shows it as the error it is.
- This client and an older daemon: `Client::attention` reads a 404 as not supported (`None`), never as an empty list.
  A missing `listing` or `state` reads as `None`.
- A newer daemon: a kind, subject or doctor state this client does not know reads as `Unknown`; a tone it does not
  know as neutral; a fix it cannot read as none. `crates/wire-compat` holds a sample of each new document.

## Consequences

- One function decides what the overview and every client's attention list say; a client lays the items out.
- A source with no current evidence stays out until it has some: sizing and caps above, and an insight read that
  fails, which is logged and lists no insight until the next read.
- The doctor reads the host every 10 minutes while the daemon runs, a few subprocesses each time.
- A request for a run that comes while one goes on may leave the worker one redundant run after it.
- Records stay a section of Jobs with their own page. Marking a job's record in the job list and filtering by it is
  part of the jobs search that follows.
