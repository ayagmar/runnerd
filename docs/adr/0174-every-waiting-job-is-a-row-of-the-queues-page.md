# 0174. Every waiting job is a row of the queue's page

Status: **Accepted** (2026-11-24). Amends [0136](0136-the-dashboard-is-laid-out-by-task.md) and
[0145](0145-the-dashboard-leads-with-the-lanes.md): the dashboard gains `/queue` and `/live/queue`. Adds to core
`Status::unlisted`, `Unlisted`, `Status::unlisted_line` and `Status::empty_queue_line`, and to `crates/fixtures` `unlisted()` and
`Facts::unlisted`. No wire or config change.

## Context

The overview's queue shows the oldest six waiting jobs, three on a phone. Past that, its **All N queued** link opened
the Jobs page filtered to queued jobs. That page is the history: it says nothing of why a job waits, and it is not
the same list. `Status::queued` leaves out a repository whose listing failed, that GitHub's rate limit stopped, or
that is not polled while the daemon stops, while the history keeps that repository's jobs queued. With 30 to 60 jobs
waiting, which a matrix push brings, every job past the sixth had no reason anywhere on the dashboard.

A queued row opened GitHub's page, or `/jobs/{id}` when the job had no GitHub URL, whether or not the history held
the job.

## Decision

- `/queue` is a page of the overview, titled *Waiting*. It lists every job of `Status::queued`, oldest first, each
  with its wait, name, place and `Waiting::reason` coloured by `WaitKind`, in the overview's row. Its heading carries
  the overview's *Waiting* health figure. `/live/queue` keeps it current: the status is read again when a job, slot,
  profile, repository, budget, cache or runner event arrives, and at the latest once it is 10 s old, event or not;
  each read renders the waits as they have grown, sending only the wait of each row while the rest is unchanged.
- The overview stays short, as 0145 has it. Its link to the rest, shown on a phone past three jobs and on a wide
  screen past six, opens `/queue`. While a repository is left out, a second link, `Status::unlisted_line` such as
  `1 repository not listed`, opens `/queue` on every screen, however few rows the overview has.
- Above the list, each repository `Status::unlisted` names gets core's line, such as `Cannot list halyard/ledger's
  queued jobs: GitHub answers 502; none shows here`, with a link to its jobs the history keeps queued. An empty queue
  says `Status::empty_queue_line`: *Nothing waiting* only while every repository is `QueueShown::Listed`; *No job of a
  listed repository waits* while one is left out; and *runnerd shows no waiting job* from a daemon that does not say
  whether it listed one (`QueueShown::Unknown`), which has none named.
- A row opens the job's page when the history holds the job, with GitHub's page as a second link; else GitHub's page;
  else it is a row the keys reach that opens nothing. Which jobs the history holds is one read of its queued jobs per
  page load and per queued, started or finished job, never a read per row, none for a step alone, and none while
  nothing is queued. It is read again too when a status read shows a queued job the last read did not ask about, as a
  repository's listing coming back with no event of its own does. When that read fails, every row opens GitHub and
  the read is tried again with each read of the status until it answers. Rows are keyed by job ID, so jobs of one name are each their own row, and a row whose job
  leaves goes without moving focus off another. On a phone the page wraps a row's name and branch rather than cut
  them; the overview keeps its rows to one line each.
- Peer machines' queues are not merged here; each machine's dashboard lists its own.

## Consequences

- A queue of any length is reachable with its reasons in two clicks or keys from the overview, on a phone too.
- The page reads the status and one page of the history's queued jobs; a long queue costs one status read per event
  batch, as the overview's does.
- A job the history holds beyond the newest 500 queued opens on GitHub, not here.
- The demo takes `RUNNERD_DEMO_QUEUED=N` for a queue of N jobs and `RUNNERD_DEMO_START_EVERY=SECS` to start the oldest
  every SECS seconds; `crates/web/tests/browser.mjs` checks 0, 3, 4, 6, 7 and 60 jobs at 1440×900 and 390×844, the
  keys, a tap, names and branches read whole on a phone, and a job leaving while another row has focus.
