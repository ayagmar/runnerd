# 0085. GitHub Actions cache saves wait for the decision, not the job

Status: **Accepted** (2026-10-30). Amends [0082](0082-github-actions-cache-served-from-the-host.md).

## Context

0082 had a save wait in `CreateCacheEntry` at most 60 s for the engine to decide whether its job may write, then
answered it with the URL whose archive is read and dropped. The engine learns which runner holds a job from
`GET /actions/jobs/{id}`'s `runner_name`, polled each `poll_interval`. In a sandbox run on 2026-10-30 that call
said the saving job was queued on no runner for minutes after it started, so each of its three saves waited 60 s and
was refused. `actions/cache` saves in a job's post-step, its last seconds: on a GitHub that lags, every save was
refused and the cache never filled, and each save held the job for a minute besides.

The decision must still come from GitHub. The runner's log names the job, but a job can write that log; it never
names who may write.

## Decision

- **A save is answered at once.** `CreateCacheEntry` from a job whose writes are undecided gets a real upload URL,
  its archive is received within the same limits as a decided job's, and `FinalizeCacheEntryUpload` answers it as
  stored, `entry_id` 0. The save names no scope until the decision: it becomes an entry only of the scope the
  decision gives, when the decision is `Allowed` with a scope of its own, under 0082's rules unchanged. Neither the
  default branch the job reads nor anything the job sends picks it. A finalized save takes no more uploads.
- **At most one entry a key.** Whether a save's job was decided before or after it finalized, it becomes an entry
  under one lock from the check that its scope lacks the key to the index, so of two saves of one key, version, scope
  and repository, the first to become one keeps it and the other is dropped. A decided job's save of a key its scope
  has, or that another decided job is saving, is still answered with the dropping URL at once.
- **Refused or never decided.** A refusal drops the job's finalized saves, and the archive and blocks of those it is
  still sending, at once and counted as refused; the rest of those saves is read, dropped and answered as Azure and
  GitHub would. A job that may write but has no scope of its own, as a run GitHub ties to several pull requests, is
  treated the same way.
- **Bounded while undecided.** A finalized save keeps the room its archive took, so an undecided job holds at most a
  quarter of `max_size`, and all jobs' uploads at most `max_size`, as 0052's held uploads do; each entry counts at
  least 4 KiB; and finalized saves count against the 1,000 saves a runner may have open.
- **After the runner ends.** A removed runner with finalized saves or held uploads, and undecided writes, keeps them
  without its socket, holding no slot; its unfinalized saves go at once. Each poll asks GitHub for the job again, up
  to 10 minutes after the runner was removed (`ENDED_WAIT`). Before this, it was asked once. If GitHub has named no
  runner by then, or failed each time, they are dropped, counted as refused, and the daemon logs a line naming the
  runner and how many uploads went. So a GitHub that lags past 10 minutes still loses those saves.
- **Restarts and reloads.** Nothing waiting for a decision survives a restart: `open` empties the uploads, and an
  adopted runner's job is decided afresh. An upload's number, in its URL, starts at the clock in microseconds, so a
  job's client that sends again to a URL it had before the restart reaches no save made since. A reload keeps each
  running job's policy and each save's size limit, as before.
- **Not chosen.** Listing a run's jobs (`/actions/runs/{id}/jobs`) for `runner_name` could name the runner sooner,
  but it reads the same records, and the engine has no such call: it would add one to `GithubApi` without showing
  it lags less. A longer wait in `CreateCacheEntry` alone holds the post-step for that long.

## Consequences

A save made in the first minutes of a job, or at its end, no longer waits for GitHub: the post-step finishes at
once, and the entry lands at the poll after GitHub names the runner. Until then it is restorable by no job, its own
included. The job page and `runnerctl caches` count saves dropped this way with the other refused ones; only the
log says why.
