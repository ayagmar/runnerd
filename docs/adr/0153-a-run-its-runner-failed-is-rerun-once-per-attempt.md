# 0153. A run its runner failed is re-run once per attempt

Status: **Accepted** (2026-11-20). Adds `[[repositories]] rerun_runner_faults`, `JobSummary::rerun_at`,
`RepositoryStatus::rerun_refused`, the attention kind `rerun-refused`, `GithubApi::attempt_jobs` and schema 27.

## Context

Since 2026-11-05, 12 jobs ended as runner faults ([ADR 0120](0120-a-job-its-runner-failed-reads-as-a-runner-fault.md)):
fretboard 8, trellis 2, glowly 2. Each needed someone to open GitHub and re-run the run before its pull request could
merge. A runner fault is Sysbox's, not the code's, and a re-run on a new runner passes. runnerd already sends re-runs
for the owner through the job actions ([ADR 0134](0134-a-job-action-is-previewed-then-sent-to-github-once.md)), with
a preview and a confirmation each time.

Three things make an automatic re-run more than calling that path from a timer:

- A restart, or a crash right after GitHub accepted, must not send it again, and neither may a second fault of the same
  attempt.
- GitHub's re-run of a run's failed jobs takes every failed and cancelled job of the attempt, on every runner. A run
  with a job that failed in its code must not be re-run: the pull request cannot merge anyway, and the job would run
  again for nothing.
- GitHub refuses a re-run while the run is in progress, and a token without Actions: write is refused every time.

## Decision

- **The rule is stateless, from GitHub's attempt.** `actions::rerun` re-runs a run only when GitHub says its current
  attempt is the one the fault happened in and it completed, every job of that attempt (`GET
  …/runs/{run}/attempts/{n}/jobs`) that did not succeed, skip or end neutral is one the history has as a runner fault,
  and the fault's attempt is the first or the second. The send is the job actions' `rerun-failed` through the same
  flights and the same permit, which reads the attempt once more with the token in hand. Once GitHub accepted, the run
  is at a later attempt, so nothing sends it again: not this daemon, not one restarted, not after a person's re-run.
- **One entry per run, in memory.** `actions::faults::Faults` keeps each followed run's newest faulted attempt and
  whether a check is due or it is settled; a second fault of the same attempt brings a waiting check forward, an
  earlier attempt's changes nothing. Runs are followed for 6 hours. A run in progress, or busy with another action, is
  read again a minute on; a check that fails (unreachable, 5xx, timeout, 409, a withdrawn send) is made again two
  minutes on, at most 5 times. An outcome GitHub did not state is retried this way, unlike an owner's request: the
  permit's read of the attempt makes it safe. The engine starts the task; it follows `job.finished` events and, at a
  start or after missing events, the history's faults of the last 6 hours not marked re-run.
- **Stored state is the history's mark only.** Schema 27 adds `jobs.rerun_at`, set on each faulted job of the attempt
  once GitHub accepted. It is what the job's page shows, and it keeps a start from reading those faults back. The rule
  does not depend on it: a mark lost to a crash leaves a run GitHub already moved on.
- **The config of the moment.** `rerun_runner_faults` is on by default, the same in every pool of a repository (the
  check refuses pools that differ), and needs `[github] job_actions`. It is part of the job actions' policy snapshot,
  so a reload that turns it off withdraws a send in flight. A run of a repository turned off or removed waits, unread,
  until a reload turns it on or it is no longer followed.
- **A refusal holds the repository for an hour.** A 401, 403 or 404, or no token, holds the repository's re-runs for
  an hour, or until the config changes; its runs wait for the hold's end, so at most one refused request goes out per
  repository and hour. `RepositoryStatus::rerun_refused` carries what GitHub refused, worded by the request's result,
  and the attention list shows it as one warning per repository with what clears it. A rate limit waits for its reset;
  a 422 leaves the run to its owner.
- **Core words it.** `JobSummary::rerun_line` says `runnerd re-ran the run's failed jobs as attempt 2, since the runner
  failed this one`; the dashboard, the TUI and `runnerctl job` show it under the outcome. The daemon's log has one line
  per re-run, and one per run left alone with why.

## Consequences

- Every client shows the line through the parity fixtures (`RUNNER_FAULT` in `runnerd-fixtures`, now re-run).
- `rerun_at`, `rerun_refused` and the attention kind are optional on the wire; the previous release reads them as
  absent, and an older client reads the kind as unknown. A 0.1.x daemon refuses a schema 27 history, as after every
  migration.
- A run is re-run at most twice by runnerd, reaching its third attempt. A fault that comes back each time is left to
  its owner.
- A run with a job on another machine that failed is never re-run: runnerd cannot tell its cause.
- Each re-run costs two reads of GitHub (the run and its attempt's jobs) and the permit's third; a run in progress costs
  one read a minute while it is followed.
- A token fixed while a repository is held is tried at the hour's end; a reload that changes nothing does not end the
  hold.
