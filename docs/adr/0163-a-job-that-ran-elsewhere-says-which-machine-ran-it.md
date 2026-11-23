# 0163. A job that ran elsewhere says which machine ran it

Status: **Accepted** (2026-11-23). Adds `jobs.elsewhere_runner` (schema 32), `JobSummary::elsewhere_runner`,
`TimelineBar::elsewhere_runner`, `Machine::of_runner` and `Machine::ran_on`.

## Context

Two machines can serve the same repositories, each with its own daemon. A job the other machine's runner took leaves
this machine's queue and closes in its history as run elsewhere ([ADR 0109](0109-a-job-that-left-the-queue-is-followed-until-github-says-it-is-gone.md)).
GitHub's answer for the job names the runner that took it (`JobDetails::runner_name`), and `Writer::elsewhere` read it
only to decide whether the job started, then dropped it. Every client showed `elsewhere`, so a person reading one
machine's history could not tell whether the other machine, a GitHub-hosted runner or some other self-hosted one ran
the job.

Since [ADR 0158](0158-a-host-has-a-name-that-its-status-and-messages-carry.md) a runner name is
`runnerd-<8 hex>-<host>-<profile>-<slot>-<secs>`, and the host segment holds no dash, so it can be read back.

## Decision

- **The history keeps the runner's name.** Closing a job as run elsewhere stores GitHub's `runner_name` in
  `jobs.elsewhere_runner` (schema step 32), as plain text cut as a title is. An empty or absent name, as for a job
  GitHub skipped or one a newer push cancelled while it was queued, stores nothing. A runner here that later takes the
  job back clears it.
- **The API carries it.** `JobSummary::elsewhere_runner` and `TimelineBar::elsewhere_runner`, optional and left out
  when absent.
- **Core words it as the outcome.** A job that ran elsewhere on a named runner reads `ran on <host>`
  (`Machine::ran_on`), where `<host>` is the segment `Machine::of_runner` finds after `runnerd-<8 hex>-` up to the next
  dash, in a name of exactly that form. Any other name, such as `GitHub Actions 12`, reads `ran on <runner name>`. A
  job without a runner name still reads `elsewhere`, or `cancelled in queue` or its conclusion as before. The kind
  stays `Ended`: the other machine's failures are not this one's. Every client shows the outcome's label, so each
  shows the machine without code of its own.

## Consequences

- A runner name from a runnerd older than ADR 0158 whose host name held a dash parses to the host's first word:
  `runnerd-…-my-laptop-app-1-…` reads `ran on my`. The name alone cannot tell that format from the new one, and the
  configuration reference already says to upgrade every machine that serves the same repositories.
- Jobs closed as run elsewhere before schema 32 have no runner name and keep reading `elsewhere`.
- A runnerd older than this one refuses a history at schema 32 (`Store::open`), as with every schema step: going back
  needs the history moved aside.
- The previous release's clients read the new JSON and ignore the field; this release reads the previous daemon's JSON
  without it.
