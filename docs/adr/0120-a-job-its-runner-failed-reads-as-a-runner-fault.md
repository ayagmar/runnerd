# 0120. A job its runner failed reads as a runner fault

Status: **Accepted** (2026-11-09). Adds a job outcome, `runner fault`, a `runner_fault` field on `JobSummary` and
schema 20.

## Context

Sysbox 0.7.1 can drop a runner's seccomp listener. From then on every `mount`, `umount2` and `openat2` in that
runner answers `ENOSYS` (`function not implemented`) until the runner ends, and a descriptor Sysbox injects can fail
as `unsafe procfs detected … invalid argument`. The job's own code did nothing wrong, and a retry in the same runner
cannot help; a rerun gets a new runner. In fretboard's 20,954 jobs, 3 failed this way (jobs 42331166731, 42492465395
and 42539128682), and each read as an ordinary `failure`, as a test or a build failing does.

## Decision

- **A failed job whose failing step's log shows the signature reads `runner fault`.** `record::runner_fault` takes
  the stored log: a step is what follows a top-level `##[group]Run …` line; the job's is a runner fault when, in a step
  that logged an error (`##[error]…`), one line has `function not implemented` with `mount` or `openat2` in it (an
  `umount2` and a `mounting "cgroup"` have `mount`), or has `unsafe procfs detected` and `invalid argument`. The same
  words in a step that did not fail, or `ENOSYS` from another call, do not count.
- **It is read once, when the job's record is final** (the writer's `close`: GitHub completed it and its runner is
  removed, so the log is complete) and kept in `jobs.runner_fault` (schema 20, `0` in older rows, which are not read
  again). `JobSummary::runner_fault` carries it; the field defaults to `false`, so the previous release reads what this
  one writes and this one reads what it wrote.
- **Core words and classes it.** `JobSummary::outcome` returns `runner fault` with `OutcomeKind::RunnerFault` for a
  job that concluded `failure` or `timed_out` and has the field. Clients only draw it, in the colour of a warning: the
  dashboard's `warn` badge, the terminal's yellow.
- **It still counts as a failure in `GET /v1/stats`, the insights and notifications**: GitHub concluded it failed,
  and the owner wants to hear of it. `is_failure` is unchanged, so an outcome's kind is `Failed` or `RunnerFault`
  exactly when the job failed.

## Consequences

- Every client shows the label through the parity fixtures (`RUNNER_FAULT` in `runnerd-fixtures`).
- A job that failed before this release stays `failure`: nothing reads old logs again.
- A log cut at `history.log_max` before the error is not read as a fault, and neither is one whose error is not a
  `##[error]` line of a step, as a runner that lost its connection leaves; both read `failure`, as before.
- The check is by words: another tool that prints them in a failing step is read as the runner's fault too.
