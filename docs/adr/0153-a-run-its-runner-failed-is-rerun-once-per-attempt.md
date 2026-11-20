# 0153. A run its runner failed is re-run once per attempt, where the config asks

Status: **Accepted** (2026-11-20). Adds `[[repositories]] rerun_runner_faults` (off by default),
`JobSummary::rerun_at`, `RepositoryStatus::rerun_refused`, the attention kind `rerun-refused`,
`GithubApi::attempt_jobs`, `RunNow::failed` and schema 27 (`jobs.rerun_at`, `rerun_claims`). The check refuses one
repository spelled in two cases.

## Context

Since 2026-11-05, 12 jobs ended as runner faults ([ADR 0120](0120-a-job-its-runner-failed-reads-as-a-runner-fault.md)):
fretboard 8, trellis 2, glowly 2. Each needed someone to open GitHub and re-run the run before its pull request could
merge. A runner fault is Sysbox's, not the code's, and a re-run on a new runner passes. runnerd already sends re-runs
for the owner through the job actions ([ADR 0134](0134-a-job-action-is-previewed-then-sent-to-github-once.md)), with
a preview and a confirmation each time.

Five things make an automatic re-run more than calling that path from a timer:

- **The evidence is the job's own output.** `record::runner_fault` reads what the failed step printed. A test of
  mount-error handling can print the same line and fail on something else. ADR 0120 accepts that for a label; a write
  with the owner's token needs more care. Nothing else runnerd can read is out of the job's reach and says more:
  - The runner's `_diag` (`Runner_*.log`, `Worker_*.log`) is in `/home/runner`, written by the runner as `runner`, the
    user job steps run as. Steps also hold the container's own Docker socket, which is root in the container, so
    nothing inside it, its exit code included, is beyond a step's reach.
  - Sysbox's journal on the host is beyond it, but is silent on the faults: of the 12 recorded from 2026-11-05 to
    2026-11-20, 11 left no warning or error there, and one (2026-11-17 02:06:47) matches an `openat2(): ... failed to
    inject fd` line by the second, which names a host pid, not a container. The same line came during runs whose jobs
    passed.
  - Docker's view of the container (exit code, OOM kill) carries no sign of either fault.
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
  else and stops an automatic request in flight. Before the permit let it go, nothing was sent and it is withdrawn;
  after, it may have reached GitHub, so its outcome is unknown, as for a broken connection.
- **The rule reads GitHub's attempt and conclusion.** `actions::rerun` re-runs a run only when:
  - GitHub says its current attempt is the one the fault happened in, and it completed as `failure`;
  - every job of that attempt that did not succeed, skip or end neutral is a runner fault in the history. The attempt's
    jobs come from `GET …/runs/{run}/attempts/{n}/jobs`, all pages or a failed read: a listing past the page cap, a
    page without `total_count`, pages that disagree on it, or fewer distinct jobs than it says, is refused;
  - the fault's attempt is the first or the second.

  The send is the job actions' `rerun-failed`, through the same flights and the same permit. With the token in hand,
  the permit reads the attempt and the conclusion once more, and the policy revision.
- **The rule binds runnerd's decision, not the write.** GitHub's re-run takes no expected attempt. A person (or another
  daemon) who re-runs the run between the permit's read and GitHub's handling of runnerd's request has that request
  apply to the new attempt, whatever its jobs did: a code failure or a cancellation there is no veto, and the attempt
  cap does not stop it. runnerd accepts that race rather than leave faults to people; closing it would take a
  precondition GitHub does not offer. The cap and the vetoes say what runnerd decides; one request can still re-run
  one attempt more than they allow.
- **Each attempt is claimed before its request, and sent once.** Before the send, runnerd writes a claim for the
  repository, run and attempt to the history's `rerun_claims`, synced to disk. A start reads no fault of a claimed
  attempt back, and a second claim of it sends nothing, so neither a restart nor a crash mid-request sends one again.
  The claim goes only when nothing reached GitHub: the permit stopped it, a reload withdrew it before the permit let
  it go, the connection was never made, or GitHub refused (a 4xx, a rate limit). Claims go after a day. Daemons with
  separate histories do not see each other's claims.
- **No send is repeated on an unstated outcome.** A timeout, a 5xx, a broken connection, or a reload after the permit
  let the request go: the request may land yet, so the claim stays. runnerd reads the run once and logs whether it
  moved, but notes nothing as its own: a moved attempt does not say who moved it. That read is rate-limited and held
  like any other.
- **One entry per run, in memory.** `actions::faults::Faults` keeps each followed run's newest faulted attempt and
  whether a check is due or it is settled. A second fault of the same attempt brings a waiting check forward, but never
  a backoff, a hold or a rate limit's wait. Runs are followed for 6 hours. A run in progress, or busy with another
  action, is read again a minute after its check ended. A check that fails is made again two minutes after it ended,
  at most 5 times. An owner's flight in progress for the run makes the automatic check wait. It never follows that
  flight, so the owner's re-run is never noted as runnerd's. The engine starts the task. It follows `job.finished`
  events and, at a start or after missing events, reads the history's faults of the last 6 hours not marked re-run.
- **Stored state is the claim and the mark.** Schema 27 adds `rerun_claims` and `jobs.rerun_at`. The mark is set on
  each faulted job of the attempt only when GitHub accepted runnerd's request, and is what the job's page shows; the
  claim is what keeps a request from going twice. Schema 26 is the wait reasons' ([ADR 0154](0154-a-job-keeps-how-long-each-cause-held-it.md)).
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
- A repository that opts in lets a workflow make runnerd re-run it by printing Sysbox's error: runnerd decides it for
  two attempts of a run at most, and a person's concurrent re-run can add one. Turning it on by default needs evidence
  outside the job's reach that the faults leave, which neither `_diag`, the container's exit, nor Sysbox's journal is.
  The candidate is a probe run as root by the container's entrypoint after the runner exits, in the process tree whose
  seccomp listener Sysbox lost, reported through the container's exit code; it is not built, and no recorded fault can
  test it.
- A run with a failed job on another machine is never re-run: runnerd cannot tell its cause.
- Each re-run costs three reads of GitHub (the run, its attempt's jobs, the permit's read), one more after an unstated
  outcome, and one synced write to the history. A run in progress costs one read a minute while it is followed.
- A token fixed while a repository is held is tried at the hour's end. A reload that changes nothing does not end the
  hold.
