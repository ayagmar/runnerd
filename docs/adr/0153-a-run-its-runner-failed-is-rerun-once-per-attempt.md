# 0153. A run its runner failed is re-run once per attempt, where the config asks

Status: **Accepted** (2026-11-20). Adds `[[repositories]] rerun_runner_faults` (off by default),
`JobSummary::rerun_at`, `RepositoryStatus::rerun_refused`, the attention kind `rerun-refused`,
`GithubApi::attempt_jobs`, `RunNow::failed` and schema 27. The check refuses one repository spelled in two cases.

## Context

Since 2026-11-05, 12 jobs ended as runner faults ([ADR 0120](0120-a-job-its-runner-failed-reads-as-a-runner-fault.md)):
fretboard 8, trellis 2, glowly 2. Each needed someone to open GitHub and re-run the run before its pull request could
merge. A runner fault is Sysbox's, not the code's, and a re-run on a new runner passes. runnerd already sends re-runs
for the owner through the job actions ([ADR 0134](0134-a-job-action-is-previewed-then-sent-to-github-once.md)), with
a preview and a confirmation each time.

Five things make an automatic re-run more than calling that path from a timer:

- **The evidence is the job's own output.** `record::runner_fault` reads what the failed step printed. runnerd has
  nothing else: it stores no runner `_diag` log, the container has exited by the time the record is final, and Sysbox's
  own logs are the host's. A test of mount-error handling can print the same line and fail on something else. ADR 0120
  accepts that for a label; a write with the owner's token needs more care.
- **GitHub's re-run names no attempt.** `POST …/runs/{run}/rerun-failed-jobs` re-runs whatever attempt is current when
  GitHub handles it. No precondition binds it to the attempt runnerd checked.
- **A send GitHub does not answer may land later.** A timeout, a 5xx or a broken connection says nothing either way.
- **Several faults, a restart, or the owner's own re-run** must not give two re-runs of one attempt, nor one recorded
  as runnerd's that the owner sent.
- **GitHub refuses** a re-run while the run is in progress, and a token without Actions: write every time.

## Decision

- **Off by default, per repository.** `rerun_runner_faults = true` turns it on. It needs `[github] job_actions` and must
  be the same in every pool of a repository, and the check refuses one repository spelled in two cases, which GitHub
  reads as one. It is part of the job actions' policy snapshot. A reload publishes the new snapshot before anything
  else, and an automatic request still waiting for its connection is withdrawn when the snapshot changes. One already
  on the wire cannot be recalled.
- **The rule reads GitHub's attempt and conclusion.** `actions::rerun` re-runs a run only when:
  - GitHub says its current attempt is the one the fault happened in, and it completed as `failure`;
  - every job of that attempt that did not succeed, skip or end neutral is a runner fault in the history. The attempt's
    jobs come from `GET …/runs/{run}/attempts/{n}/jobs`, all pages or a failed read: a listing past the page cap, or
    with fewer jobs than its `total_count`, is refused;
  - the fault's attempt is the first or the second.

  The send is the job actions' `rerun-failed`, through the same flights and the same permit. With the token in hand,
  the permit reads the attempt and the conclusion once more, and the policy revision. Once GitHub accepted, the run is
  at a later attempt, so nothing sends it again: not this daemon, not one restarted, not after a person's re-run.
- **The race with a person is bounded, not closed.** A person who re-runs the run between the permit's read and
  GitHub's handling of runnerd's POST gets one more re-run of the new attempt. The attempt cap bounds it. Closing it
  would take a precondition GitHub does not offer.
- **No send is repeated on an unstated outcome.** For a timeout, a 5xx or a broken connection, runnerd reads the run
  once. If its attempt moved, the re-run landed and is noted; otherwise the run is left to its owner, with a log line.
  A request that never left (unreachable, withdrawn) or that GitHub refused with a 409 is checked again later. The
  permit reads the run before any second send.
- **One entry per run, in memory.** `actions::faults::Faults` keeps each followed run's newest faulted attempt and
  whether a check is due or it is settled. A second fault of the same attempt brings a waiting check forward, but never
  a backoff, a hold or a rate limit's wait. Runs are followed for 6 hours. A run in progress, or busy with another
  action, is read again a minute after its check ended. A check that fails is made again two minutes after it ended,
  at most 5 times. An owner's flight in progress for the run makes the automatic check wait. It never follows that
  flight, so the owner's re-run is never noted as runnerd's. The engine starts the task. It follows `job.finished`
  events and, at a start or after missing events, reads the history's faults of the last 6 hours not marked re-run.
- **Stored state is the history's mark only.** Schema 27 adds `jobs.rerun_at`, set on each faulted job of the attempt
  once a re-run landed. It is what the job's page shows, and it keeps a start from reading those faults back. The rule
  does not depend on it: a mark lost to a crash leaves a run GitHub already moved on. Schema 26 is the wait reasons'
  (#331), which lands first.
- **Refusals hold, rate limits stop.** A 401, 403 or 404, or no token, holds the repository's re-runs for an hour, or
  until the config changes, whether a read or the send was refused. Its runs wait for the hold's end, so one refused
  request per repository and hour goes out. `RepositoryStatus::rerun_refused` carries what was refused, worded by core,
  and the attention list shows it as one warning per repository with what clears it. A rate limit GitHub answered,
  with a reset or as a bare 429 (a minute), stops every run's reads and sends until it resets. A 422 leaves the run to
  its owner.
- **Core words it.** `JobSummary::rerun_line` says `runnerd re-ran the run's failed jobs as attempt 2, since the runner
  failed this one`. The dashboard, the TUI and `runnerctl job` show it under the outcome. The daemon's log has one line
  per re-run, and one per run left alone with why.

## Consequences

- Every client shows the line through the parity fixtures (`RUNNER_FAULT` in `runnerd-fixtures`, now re-run).
- `rerun_at`, `rerun_refused` and `RunNow::failed` are optional or internal. The previous release reads the first two
  as absent and an older client reads the attention kind as unknown, a break recorded in wire-compat's `ACCEPTED`.
  A 0.1.x daemon refuses a schema 27 history, as after every migration.
- A repository that opts in lets a workflow make runnerd re-run it, at most twice per run, by printing Sysbox's
  error. Turning it on by default needs evidence outside the job's output: a check of the runner's container before it
  is removed, or Sysbox's own log on the host.
- A run with a failed job on another machine is never re-run: runnerd cannot tell its cause.
- Each re-run costs three reads of GitHub (the run, its attempt's jobs, the permit's read) and one more after an
  unstated outcome. A run in progress costs one read a minute while it is followed.
- A token fixed while a repository is held is tried at the hour's end. A reload that changes nothing does not end the
  hold.
