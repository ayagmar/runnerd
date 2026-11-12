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

`GET /v1/attention` answers an `Attention`: an `instance`, a `revision`, up to `ATTENTION_MAX` (50) items and the count `omitted`
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
  waits for it now. The insight is then not listed again. Its subject is the host and its evidence time unknown: the
  insight was read earlier and names no job, and the job waiting now may be another that has waited seconds.
- **Doctor**: each failing (`bad`) or warning (`warn`) check of a run for the config the daemon runs with. A doctor
  that has not finished, failed or is stale gives one `doctor-unavailable` item in its place, never an empty list.
- **Insights**: those of `warning` severity, except the host's own (above).
- **GitHub**: `RepositoryStatus.listing` says why a repository's queued jobs cannot be listed, as a class and an HTTP
  status, never GitHub's words. A missing or refused token is one `github-auth` item for every repository; any other
  failure one `github-listing` item per repository. The engine sets it on a failed listing, keeps when the failures
  began, and clears it on an answer or the repository's removal; a restart forgets it. The daemon's log says the same
  class and status, never GitHub's words, which could echo what runnerd sent. A rate limit used up is one
  `rate-limited` item while `Status::github_limited_until` is still ahead, whether or not the engine published since.
- **Sizing and caps** give no item: no source says now that a job is blocked by its profile's size. The sizing page
  says what it advises; an item would repeat it without new evidence.

Items are ordered by tone (bad, warn, neutral, quiet, good), then those with an evidence time before those without,
newest first, then by key. Nothing tracks when a condition began beyond what its source already keeps.

### Liveness

The daemon's attention worker owns the snapshot. It looks again when the status, the doctor's run or the config
changes, on each durable event, and every 30 seconds, so a condition that comes or goes with time alone shows within
that bound. It reads the insights at most once a minute, and again at once when the config they were read with is no
longer the one running or other jobs wait for the host's memory than when they were read. The revision moves only when
the content does, also when one item replaces another at the same count, and each move is sent on `/v1/events` as the
live event `attention.changed {instance, revision}`. A client refetches on that event, or polls. The event is live:
it has no event ID, so a client resuming with `Last-Event-ID` neither misses nor repeats one, and reads the snapshot
when it connects.

`instance` names the daemon process: new at each start, never one an earlier process had. `revision` counts from 1
within it. A client compares revisions only within one instance; a snapshot of another instance is new, whatever its
revision. A revision started from the clock would go back when the clock steps back before a restart, and a client
holding a higher one would ignore every change until the count caught up. Nothing of the snapshot survives a restart:
the first one is computed from the status with the doctor not yet run, so it is never an all clear.

A config replaced now reaches each of its readers on its own: a `watch` of the config's revision replaces the single
`Notify`, and the notifier, the doctor worker and the attention worker each hold a receiver.

### The doctor's cache

One worker runs the doctor's checks, one run at a time: at the start, after each config replaced, when a request finds
no run of the current config under a minute old, and every 10 minutes. Its last run is a sealed value: not run, or done
for a config revision at a time with its checks or its error. Reloads that come during a run make one run after it, a
run whose config was replaced while it ran is dropped, and requests that came during a run are answered by it without a
second.

A run reads the job history the sizing check needs from the store, then everything it asks the host, the slice's
`MemoryMax` included, on one blocking thread. One deadline of 2 minutes covers the whole run; a run past it is reported
failed, and the next starts only once its blocking work ended. Each command (`systemctl`, `gh`) runs in a process group
of its own: past 20 seconds the whole group is killed, and the 20 seconds cover reading its output, so a process it left
holding its pipes holds nothing. Each Docker request is dropped past 20 seconds. At a stop the worker ends, and the
daemon kills every command group still running and starts no other. A run older than 15 minutes, or of a config no
longer running, is stale.

`GET /v1/doctor` reads the same cache, with `Doctor::state` (`not-run`, `ready`, `stale`, `failed`): a request that
finds no recent run asks the worker for one and waits for it at most 20 seconds, under the client's 30-second timeout.
A run not done by then goes on, and the request answers the last run as it stands: 503 with the state's line while no
run has finished or the last failed, else the checks marked stale. `runnerctl doctor` prints the state's line above
checks that are not current and exits non-zero; the dashboard shows the line and paints none of them as passing or
needing a fix. `runnerd doctor` on the command line still reads the host itself. Nothing of the cache survives a
restart.

### Five views

`api::View` is Overview, Jobs, Insights, Capacity and Settings. Each lists its `api::Section`s: Jobs holds History and
Records, Capacity holds Sizing and Caches, Settings holds Config. Every page of before is reachable in its view: the
dashboard's navigation names the five views and, on a view of several sections, links each; the TUI's digit opens a
view's first section it has and the same digit again the next. `View` and `Tone` are wire data now, for `Fix::Open`
and `AttentionItem::tone`.

"Jobs" now names a view, and `check-boundaries.sh` fails on a client that writes a view's or a section's name as a
literal, with no exception. A count or a list of jobs is headed `api::JOBS_HEADING`, which reads the same but is not
the view's name.

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
- Records stay a section of Jobs with their own page. Marking a job's record in the job list and filtering by it is
  part of the jobs search that follows.
