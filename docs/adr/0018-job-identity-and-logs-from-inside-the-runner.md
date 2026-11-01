# 0018. Job identity and logs from inside the runner container

Status: **Accepted**.

## Context

A JIT runner's container does not say which job its runner took, and GitHub's list of queued jobs lags the runner
taking one by several seconds. Starting a runner for each job still listed started twins that took no job. GitHub's
log API returns a job's log only once it finished.

## Decision

Once a runner container starts, the daemon runs a script in it (`docker exec`, as root, `observe::capture`) that
reads the job's ID from the runner's `_diag/Worker_*.log` and follows the job-log pages the runner writes under
`_diag/pages`, hard-linking each so the runner deleting it after upload does not lose lines. The engine counts a job
its runner holds as taken whatever GitHub's queue says. Step names and times come from GitHub's jobs API instead,
since the Worker log uses internal step names.

## Consequences

Logs arrive while the job runs, in the runner's 4 KiB blocks, with the secrets the runner masks already masked. The
script depends on the runner's `_diag` layout; a runner upgrade that changes it shows up as jobs with no log and no
ID until the script is updated. The capture resumes from the stored page and offset after a daemon restart.
