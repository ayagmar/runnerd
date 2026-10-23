# 0024. Pauses are recorded in the job history and survive restarts

## Context

A pause lived only in `Control`'s memory, so a daemon restart, planned or not, resumed every repository: an owner who
paused a repository to keep a broken workflow or a busy machine quiet got runners for it again after an upgrade. Nor
was there a way to pause every repository at once short of one `runnerctl pause` each or stopping the daemon, which
also stops the API and the dashboard.

## Decision

The job history's SQLite database gets a `paused` table (migration 3), one row per paused repository. `Control`
writes the whole set through the store's queue while it holds the lock that changes it, so the rows change in the
same order as the set, and answers a pause or resume once that write commits. A failed write is logged and the
pause holds until a restart. The daemon reads the table at startup and the engine starts with those repositories
paused, before its first tick.

A repository the config no longer has loses its pause, at a reload and at startup: removing it is the owner telling
runnerd to stop serving it, and a pause kept out of sight would surprise whoever adds it back later. Added back, it
starts active.

`POST /v1/repositories/pause?force=` and `POST /v1/repositories/resume` pause and resume every configured repository
in one engine command. Without `force`, the pause is refused with the jobs of every repository that runs one and
pauses none; with it, it cancels their runs as a forced pause of one repository does (ADR 0019).

## Consequences

The pause state lives in the database that `history.database` names: moving that database aside, or pointing the
config at a new one, resumes every repository. The rows are not events and are never pruned. Clients learn of a
restored pause the way they learn of any other, from `repository.changed` and the status.
