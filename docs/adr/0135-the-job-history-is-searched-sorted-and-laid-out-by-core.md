# 0135. The job history is searched, sorted and laid out by core

Status: **Accepted** (2026-11-12). Extends `GET /v1/jobs`; adds `GET /v1/timeline`, `GET /v1/jobs/{id}/compare`, the
`record.skipped` event and the types in `api::jobs::query`, `api::jobs::timeline` and `api::jobs::compare`. Schema
stays 22.

## Context

The Jobs view lists the history newest first by ID and filters it by exact repository, state, conclusion, workflow
and name. It cannot find a job by part of its name or branch, sort by how long jobs waited or ran or how much memory
they took, show what each slot ran over a day, or say how a run differs from the last good run of the same job. A
failed job's record shows only on its own page; the records page lists Docker's images.

Each of these is easy to get subtly wrong in a client: pages that repeat or skip rows when a sort key is unknown or
equal, a cursor reused after the filters changed, a timeline that calls time runnerd did not watch idle, or steps
paired by position after a rename. ADR 0062 puts those decisions in core.

## Decision

**One meaning, two implementations.** Each read's meaning is a pure function in `api`, which builds without `host`:
`JobQuery::page`, `SlotTimeline::of` and `JobComparison::in_history` over a history held in memory. The store reads
the same answer in SQL, bounded at the read. Tests compare the two over generated histories, with restarts, every
filter, sort, order and page size. The demo's fake daemon and `crates/fixtures` use the pure versions.

**Search and filters.** `q` is a literal substring of 1 to 100 bytes, no control characters, matched against the
job's name, workflow and branch, ASCII letters in either case (`LIKE … ESCAPE '\'`, with `%`, `_` and `\` escaped).
`branch` matches exactly. `record` keeps jobs whose record was `kept`, `removed` or `skipped`, or that have any note.
Commit search is left out: the history does not store the commit.

**Sorts and the cursor.** `sort` is `queued`, `wait`, `run` or `memory`, `order` `desc` (the default) or `asc`. A
key is unknown until it is final: wait until the job starts, run and memory until the job is final. Unknown keys sort
last in both orders; ties break by ID in the sort's order. A key only goes from unknown to known, so one pass through
the pages never shows a job twice; one that becomes known behind the cursor waits for the next pass. A sorted page
answers `cursor`, `j1.<sort>.<order>.<filter>.<key>.<id>`, where `<filter>` is a hash of every filter. A cursor of
another sort, order or filter is refused with 400 and core's words. The default order keeps `before` and `next`.
`before` with `sort`, or `cursor` without it, is refused.

**Bounds.** A page is at most `limit` rows (50 by default, 500 at most) and 1 MiB of its rows' text, measured in the
`SELECT` that picks the rows, and still answers `next` or `cursor` when cut by bytes. A page is read in one read
transaction, so pruning in the middle of it cannot mix two states of the history.

**The answer says what it read.** `GET /v1/jobs` answers `JobList`: `JobPage`'s `jobs` and `next`, with `cursor` and
`applied`, the query as the daemon read it. A daemon before this one ignores `q`, `sort` and the rest and answers no
`applied`; `JobList::understood` is false, and a client must not show its list as a search result.

**The record mark.** `JobSummary::record_mark` is `kept`, `removed` or `skipped`, from the history's record note in
`RecordNote::state`'s precedence, read in the same `SELECT` as the row. Core words it and gives its tone. It is not
named `record`: a `Job` flattens its summary and already has `record`, the note in full. Every change to a note
announces an event: `record.created`, `record.deleted`, and the new `record.skipped`. The mark is what the history
says; Docker's images stay the inventory at `GET /v1/records`. An image removed outside runnerd, with `docker rmi`,
is not noted, so its job still reads `kept`. A record whose job the history no longer has is in `GET /v1/records`, with its
repository, profile, workflow and name from the image's labels; it is in no job list and its `GET /v1/jobs/{id}`
answers 404.

**The timeline.** `GET /v1/timeline?since&until` lays out a window of at most 7 days (the last 24 hours by default)
by the slot each runner had when it ran, not by today's config, so a lane of a profile since removed or resized is
still there. A job is in the window when it was queued before its end and ended after its start, is still queued or
running, or began in it with an end the history lacks. Bars keep their real ends (`at`, `open` or `unknown`); a job
no runner here took is in `unassigned`. `observed` lists when runnerd was watching, from its host samples no more than
three intervals apart, the samples not yet stored included, never before `history_from`, the oldest time from which
the history keeps every job. `Lane::gaps` words the rest of a lane: `no job` where runnerd was watching, `unknown`
where it was not or where a bar's start or end is unknown. No downtime is inferred and no reservation is drawn. At
most 2,000 bars and 2,000 runner spans are read, the latest kept; `cut_before` says from when some are left out. Job
and workflow names are cut in SQL and made safe by core before it lays them out.

**The comparison.** `GET /v1/jobs/{id}/compare` compares a job with the latest earlier run of the same job: the same
repository, workflow, name and branch, completed `success` with a runner here, earlier by queued time, then ID. A job
whose workflow or branch is unknown has no baseline, said as `unavailable` with the reason; so has the first run.
`?with=` names the baseline; a job of another name answers `incompatible`. Whole-job figures are given only for a final
job. Steps pair by exact name when the name is unique on both sides; two steps of one name are `ambiguous`, a renamed
step is `only this run` on one side and `only the baseline` on the other. Never by position. Step times come from the
stored steps; a step's memory peak and average CPU come from samples, and only when both sides have at least half the
samples the step's length implies (`sampled`). Past sample retention a figure is unknown, never zero. A change is
`Warn` or `Good` past 10% and a floor (5 s for times) for costs only; unknown reads `Quiet`.

**Two versions.** This daemon with a client of 0.1.0: new fields and the `record.skipped` event are ignored, `JobPage`
still reads. A new client with a 0.1.0 daemon: the list is not `understood`, `/v1/timeline` and `/compare` answer
`404 no such endpoint`, which `ClientError::is_unsupported` reads as not supported. Every new enum reads a value it
does not know as `Unknown` or `Other`.

## Consequences

- No migration. On a history of 200,000 jobs a sorted page took 19 to 29 ms, a search that matches nothing 37 ms
  and a week's runner scan 21 ms, without a new index; none pays for its write cost at that size.
- A sort by wait reads every row its filters admit; a history far past 200,000 jobs may want an index on the key.
- The mark can disagree with Docker for a record removed outside runnerd; the records page, which lists the images,
  is the one to act from.
- A step renamed between two runs compares as two steps, and so do two steps of one name; matching them better needs
  GitHub to say which step is which, which it does not.
