# 0135. The job history is searched, sorted and laid out by core

Status: **Accepted** (2026-11-12). Extends `GET /v1/jobs`; adds `GET /v1/timeline`, `GET /v1/jobs/{id}/compare`, the
`record.skipped` event and the types in `api::jobs::query`, `api::jobs::timeline` and `api::jobs::compare`. Schema
23: daemon sessions and a record's `gone_at`.

## Context

The Jobs view lists the history newest first by ID and filters it by exact repository, state, conclusion, workflow
and name. It cannot find a job by part of its name or branch, sort by how long jobs waited or ran or how much memory
they took, show what each slot ran over a day, or say how a run differs from the last good run of the same job. A
failed job's record shows only on its own page; the records page lists Docker's images.

Each of these is easy to get subtly wrong in a client: pages that repeat or skip rows when a sort key changes between
two reads, a timeline that calls time runnerd did not watch idle, or steps paired by position after a rename. ADR 0062
puts those decisions in core.

## Decision

**One meaning, two implementations.** Each read's meaning is a pure function in `api`, which builds without `host`:
`JobQuery::page`, `SlotTimeline::of` and `JobComparison::in_history` over a history held in memory. The store reads
the same answer in SQL, bounded at the read. Tests compare the two over generated histories, with restarts, every
filter, sort, order and page size. The demo's fake daemon and `crates/fixtures` use the pure versions.

**Search and filters.** `q` is a literal substring of 1 to 100 bytes, no control characters, matched against the
job's name, workflow and branch, ASCII letters in either case (`instr(lower(column), lower(q))`, which reads past a
NUL where `LIKE` stops). `branch` matches exactly. `record` keeps jobs whose record was `kept`, `removed` or `skipped`, or that have any note.
Commit search is left out: the history does not store the commit.

**Sorts and pages.** `sort` is `queued`, `wait`, `run` or `memory`, `order` `desc` (the default) or `asc`. A key is
unknown until it is final: wait until the job starts, run and memory until the job is final. Unknown keys sort last in
both orders; ties break by ID in the sort's order. The default order, newest first, pages by `before` and `next`, as
before. Any other order is one page: its first `limit` jobs, read in one snapshot, with `total`, how many the filters
admit, and `JobList::shown_words`, such as `50 of 1204 jobs, longest wait first`. It has no continuation. A key
changes while the history is read (a job starts, a running job ends), so a cursor over it repeats or skips jobs unless
it is bound to a revision of the history; a sort is for finding the extremes, which the first page holds, and the
search and filters narrow it. `before` outside the default order, and `cursor`, are refused with 400 and core's words.

**Bounds.** A page is at most `limit` rows (50 by default, 500 at most) and 1 MiB of its rows' text. Each text column
is read as its first 1,024 bytes (`JOB_TEXT_MAX`), cut to its last whole character, and labels past 4 KiB as none, in
the `SELECT` itself; `shortened` lists the jobs so cut. The byte budget is checked against the row about to be read,
so a page always has its first row and ends before the row that would pass it, still answering `next`. A page is read
in one read transaction, so pruning in the middle of it cannot mix two states of the history.

**The answer says what it read.** `GET /v1/jobs` answers `JobList`: `JobPage`'s `jobs` and `next`, with `total`,
`shortened` and `applied`, the query as the daemon read it. A daemon before this one ignores `q`, `sort` and the rest
and answers no `applied`; `JobList::understood` is false, and a client must not show its list as a search result.

**The record mark.** `JobSummary::record_mark` is `kept`, `removed` or `skipped`, from the history's record note in
`RecordNote::state`'s precedence, read in the same `SELECT` as the row. Core words it and gives its tone. It is not
named `record`: a `Job` flattens its summary and already has `record`, the note in full. Every change to a note
announces an event: `record.created`, `record.deleted`, and the new `record.skipped`. The mark is what the history
says, kept true by Docker's listing: each time the engine lists the record images and Docker answers, the store's
writer notes every record noted kept whose image is not listed as gone (`RecordNote::gone_at`, when runnerd found it
gone), marks it `removed`, words it `removed outside runnerd, when and why unknown` and announces `record.deleted` with
`removed_by` `outside`. The writer does it after every write sent before, so a record runnerd just made or removed is
noted first. A listing Docker did not answer changes nothing. A record whose job the history no longer has is in `GET /v1/records`, with its
repository, profile, workflow and name from the image's labels; it is in no job list and its `GET /v1/jobs/{id}`
answers 404.

**The timeline.** `GET /v1/timeline?since&until` lays out a window of at most 7 days (the last 24 hours by default)
by the slot each runner had when it ran, not by today's config, so a lane of a profile since removed or resized is
still there. A job is in the window when it was queued before its end and ended after its start, is still queued or
running, or has an end the history lacks while its runner was there after the window's start (or, with no runner,
began in it). Bars keep their real ends (`at`, `open` or `unknown`); a job
no runner here took is in `unassigned`. `observed` lists when runnerd was watching: inside a daemon session, from its
host samples no more than three intervals apart, the samples not yet stored included, never before `history_from`,
the oldest time from which the history keeps every job. A session is a row of `sessions`, written when the store opens
and moved on with each host sample it stores, pruned with the samples; a restart ends one, however short, and time
before the first session is unknown. `Lane::gaps` words the rest of a lane: `no job` only where runnerd was watching
and the history is whole, `unknown` where it was not, where some bars were left out (`cut_before`), or where a job of
the lane's runners has an end the history lacks, up to that runner's end. No downtime is inferred and no reservation is drawn. At
most 2,000 bars and 2,000 runner spans are read, the latest kept; `cut_before` says from when some are left out. Job
and workflow names are cut in SQL and made safe by core before it lays them out.

**The comparison.** `GET /v1/jobs/{id}/compare` compares a job with the latest earlier run of the same job: the same
repository, workflow, name and branch, completed `success` with a runner here, earlier by queued time, then ID. A job
whose workflow or branch is unknown has no baseline, said as `unavailable` with the reason; so has the first run.
`?with=` names the baseline; a job of another name answers `incompatible`. Whole-job figures are given only for a final
job. Steps pair by exact name when the name is unique on both sides; two steps of one name are `ambiguous`, a renamed
step is `only this run` on one side and `only the baseline` on the other. Never by position. Step times come from the
stored steps; a step's memory peak and average CPU come from samples, and only when both sides' samples cover the step
from start to end: the first and last within three sample intervals of its start and end and none further apart
(`sampled`). A step partly pruned, or run while sampling stopped, has none, since its peak may be in the part not
sampled. Past sample retention a figure is unknown, never zero. The baseline is picked from the summaries first; steps
and samples are read only for a comparison that has one: the first 500 steps of each job, while their names, each read
as its first 800 bytes, add up to 128 KiB. `steps_cut` says when steps were left out. A change is
`Warn` or `Good` past 10% and a floor (5 s for times) for costs only; unknown reads `Quiet`.

**Two versions.** This daemon with a client of 0.1.0: new fields and the `record.skipped` event are ignored, `JobPage`
still reads. Its client drops a `record.deleted` with `removed_by` `outside`, which it cannot read, as it drops every
event it cannot read, and reads a gone record's note as kept, as it did; the wire check accepts that one break. A new client with a 0.1.0 daemon: the list is not `understood`, `/v1/timeline` and `/compare` answer
`404 no such endpoint`, which `ClientError::is_unsupported` reads as not supported. Every new enum reads a value it
does not know as `Unknown` or `Other`.

## Consequences

- Migration 23 adds `sessions` and `records.gone_at`. A history from before it has no session, so its old samples read
  as unwatched once opened; a downgraded daemon does not open a history of a newer schema.
- On a history of 200,000 jobs a sorted page took 19 to 29 ms, a search that matches nothing 37 ms and a week's runner
  scan 21 ms, without a new index; none pays for its write cost at that size. A sort by wait reads every row its
  filters admit; a history far past 200,000 jobs may want an index on the key.
- A sorted list cannot be read past its first 500 jobs; the search and filters narrow it instead.
- A record removed outside runnerd reads `kept` until the next listing Docker answers, which is at the daemon's start
  (or the first tick after, or the first failure to keep, if Docker did not answer then). Listing again while the
  daemon runs would race the records being committed in the background; it is left out.
- A step renamed between two runs compares as two steps, and so do two steps of one name; matching them better needs
  GitHub to say which step is which, which it does not. Two names alike in their first 800 bytes pair as one.
