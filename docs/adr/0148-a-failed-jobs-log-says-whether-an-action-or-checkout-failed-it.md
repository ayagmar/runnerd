# 0148. A failed job's log says whether an action or its checkout failed it

Status: **Accepted** (2026-11-20). Adds `FailureCause`, a `failure_cause` field on `JobSummary` and schema 25.

## Context

On 2026-11-17, 44 fretboard jobs failed in 90 minutes. 21 of them failed in `Set up job`, where the runner downloads
every action the job uses from codeload.github.com; others failed in `actions/checkout`, fetching the repository. Each
read as an ordinary `failure`, as a test failing does, and none as a runner fault ([ADR 0120](0120-a-job-its-runner-failed-reads-as-a-runner-fault.md)
knows only Sysbox's signatures). The owner could not tell at a glance which failures were the network's and which
the code's, nor see whether runnerd's own archives ([ADR 0147](0147-runners-read-verified-action-archives-from-runnerds-cache.md))
cut the first kind down.

The runner 2.338.1 (`ActionManager.cs`, `JobExtension.cs`) reports its own failures in `Set up job`. A download that
fails is retried three times; each failed attempt but the last prints `##[warning]Failed to download action '…'.
Error: …` and `##[warning]Back off … seconds before retry.`. The last one prints the attempt's own message as an
error, which may say nothing of a download (`Response status code does not indicate success: 504`), and then
`JobExtension` ends the step with the exception's message: `##[error]Failed to download archive '…' after 3
attempts.`. An archive `tar -xzf` cannot unpack ends it with `Can't use 'tar -xzf' extract archive file: …`. An action
GitHub's API does not resolve prints `Failed to resolve action download info. Error: …` and `Retrying in … seconds`
before each retry, and ends with `##[error]Failed to resolve action download info.`. A retry that succeeds prints
nothing, so its warnings stay in the log above whatever fails next. The categories the runner files these under
(`resolve_action`, `error_download_action`, `invalid_action_download`) are not in the log.

## Decision

- **A failed job's log is read for one of three causes**, by `record::failure_cause`, beside `runner_fault`: only the
  step GitHub reports as `failure` counts, and only a diagnostic that ends it.
  - `action-archive`: the failed step is `Set up job`, and the last `##[error]` it printed, the one `JobExtension`
    ends it with, names a download or an archive (`Failed to download archive '…' after 3 attempts.`, `Failed to
    download action`, `Can't use 'tar -xzf'`, `Can't un-zip archive file`, `download has timed out`).
  - `action-resolve`: the failed step is `Set up job`, and that last error is `Failed to resolve action download
    info.` or `Unable to resolve action`.
  - `checkout`: the failed step's own section of the log runs `actions/checkout@…`. Its section is the one at its
    place among the steps that ran, and only when the log has exactly one `##[group]Run` section a step before its
    post steps, so a composite action's inner groups or a step's own groups leave it unknown. The section must run
    the action, not a script (no `shell:` line in its group), print the checkout's `Syncing repository:` and an
    `##[error]`. A post step never counts.
  Anything else has no cause. A retry's warnings never count, whether the retry then succeeded or not: a download or
  resolve that recovered prints the same warnings, and what fails after it is something else.
- **It is read once, when the job's record is final**, and kept in `jobs.failure_cause` (schema 25, NULL in older rows,
  which are not read again). A 0.1.4 daemon refuses a schema 25 history, as after every migration.
- **`runner_fault` stays as it is.** It decides the outcome (`runner fault`); a cause does not change the outcome, the
  stats or what counts as failed. A job can have both.
- **Core words it** (`FailureCause::line`, `JobSummary::failure_line`): the job's page in the dashboard, the TUI and
  `runnerctl job` shows the line under the outcome, and only for a job that failed. The field is optional and
  skipped when empty, and `FailureCause` has an `unknown` fallback, so the previous release and this one read each
  other's JSON.

## Consequences

- Every client shows the line through the parity fixtures (`ACTION_ARCHIVE` in `runnerd-fixtures`); the logs the
  rules are tested on are in `crates/fixtures/logs`.
- The rules are by words the runner prints today. A runner that words its failures differently reads as no cause,
  never as a wrong one, until the rules follow it.
- A log cut at `history.log_max` before `Set up job`'s end reads as no cause.
- A failure in `Set up job` that is neither, such as a Docker action's image that would not pull, has no cause.
