# 0109. A job that left the queue is followed until GitHub says it is gone

Status: **Accepted** (2026-11-08). No wire, schema or config change.

## Context

The history records a job `queued` when a listing shows it. Once it leaves the listing, or its repository leaves the
config, the engine asks GitHub each tick what became of it (`follow_jobs`) and closes the record when GitHub says another
machine took it or it was cancelled. A failed read only warned. So a job GitHub no longer answers for stayed `queued` and
was read every tick, with no end: one request and one warning per job per tick, 240 an hour at the default poll. A restart
followed it again, as `recover` takes back every open `queued` row, and prune keeps open rows.

GitHub stops answering for a job when its run or repository is deleted or transferred, or when the token loses the
repository. The last is the usual case: the owner removes a repository from the config because the token can no longer
read it, and its queued jobs then go down the removed repository's path. Twenty such jobs use up the 5,000 requests an
hour GitHub allows.

The jobs of removed runners already had an end: `closing` records them lost after `LOST_AFTER`, 15 minutes.

## Decision

- **A 404 or 410 for a job that left the queue ends its following at once.** GitHub says it is not there for this token,
  and asking again gets the same answer (`github::Error::is_gone`).
- **Any other failure ends it after `LOST_AFTER` of failed reads in a row.** The clock starts at the first failed read
  stops at any answer, and starts over whenever a tick does not read the job: a listing showed it again, its
  repository's listing failed, or the read hit the used-up rate limit.
- **Its record closes as run elsewhere, with no conclusion** (`Store::unanswered`), at the time the engine gave up. None
  of this machine's runners took it, and the job reads `elsewhere`, as core already words a job that left the queue for a
  place it cannot name.
- **A runner here that takes such a job takes its record back.** `Store::claimed` turns a row closed this way (state
  `elsewhere`, no conclusion, no start, a finish time) into a running one, as it does a queued one. A row GitHub answered
  with no runner has no finish time and stays closed, since a job's log can name any ID. GitHub can still queue a job it
  refused to show: a repository whose token lost it and that is re-added with access restored keeps its jobs queued for
  up to 24 hours, and a listing can show a job again after its reads failed.

## Consequences

- A job GitHub refuses costs one request, a job it fails to answer at most 15 minutes of them.
- A restart starts the 15 minutes again for a job whose reads fail without a 404; one that gets a 404 is closed before it.
- The record keeps no reason: a job closed this way looks like one another machine ran without a start time. While
  GitHub still queues it, the history shows it `elsewhere` until a runner takes it; one of this machine's takes the
  record back, another machine's leaves it closed.
