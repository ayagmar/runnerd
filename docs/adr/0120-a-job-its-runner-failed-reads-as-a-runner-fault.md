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

- **A failed job whose failed step printed the signature reads `runner fault`.** `record::runner_fault` takes the job
  and its stored log. It finds the step GitHub reports as `failure` (the first), and its section of the log as a
  record's shell does (`section_of`), and reads what that step printed: not the `##[group]Run …` title, the script and
  the environment the runner echoes, not a step that passed or was allowed to fail, and not what follows `Post job
  cleanup.`. A line is the signature when it has `function not implemented` and, as a word of its own, `mount`,
  `umount`, `umount2`, `unmount`, `mounting` or `openat2`, or has `unsafe procfs detected` and `invalid argument`;
  case does not matter, as glibc capitalises what Go does not. Words that hold `mount` (`amount-api`, `mounted`) do not
  count.
- **Core words and classes it.** `JobSummary::outcome` returns `runner fault` with `OutcomeKind::RunnerFault` for a
  job that concluded `failure` or `timed_out` and has the field. Clients only draw it, in the colour of a warning: the
  dashboard's `warn` badge, the terminal's yellow.
- **It still counts as a failure in `GET /v1/stats`, the insights and notifications**: GitHub concluded it failed,
  and the owner wants to hear of it. `is_failure` is unchanged, so an outcome's kind is `Failed` or `RunnerFault`
  exactly when the job failed.

## Consequences

- Every client shows the label through the parity fixtures (`RUNNER_FAULT` in `runnerd-fixtures`).
- A job that failed before this release stays `failure`: nothing reads old logs again.
- A log cut at `history.log_max` before the error reads `failure`, and so does a job whose failed step cannot be
  found in the log: one named by its author whose section the exit code or its place does not give, or one that prints
  its own `::group::Run …` line before the signature, which ends its output there. A job that concluded `timed_out`
  with no step reported as `failure` reads as it did.
- A step allowed to fail (`continue-on-error`) is skipped only if GitHub reports it as other than `failure`.
- The check is by words: another tool that prints them in the failed step is read as the runner's fault too.
