# 0118. A followed job is in one stage

Status: **Accepted** (2026-11-10). No wire, schema or config change.

## Context

The engine follows two kinds of job on GitHub that no runner of this machine runs: queued jobs a listing recorded,
until GitHub says another machine ran them ([ADR 0109](0109-a-job-that-left-the-queue-is-followed-until-github-says-it-is-gone.md)),
and jobs whose runner went, until GitHub says they completed or `LOST_AFTER` passes. They were two maps,
`queued_jobs` and `closing`, written from `history`, `tick` and `recover`. Nothing kept a job out of both. A job
whose runner went while GitHub still listed it queued was in both, and once the listing dropped it the engine read it
twice a tick. Its own claims on listed jobs (`Claims`) were a third map whose fields `tick` wrote directly.

A read of a queued job waits on GitHub while a pass runs. A pass can close that job in the meantime: its runner
reports it and stops. Whatever then handles the answer must not touch the closing job.

## Decision

- `engine/ledger.rs` holds `Ledger`: one stage per job id, `Queued { unanswered_since }` or
  `Closing { since, since_ms }`, each with its repository. Only its methods write it.
- Each method names the stage it moves from, so an answer that lands after the stage changed does nothing:
  - `follow_queued` adds a job as queued only if it is not followed yet. A queued job keeps its clock, and a
    closing one stays closing.
  - `stop_queued`, `answered` and `read_failed` act only on a queued job.
  - `close` moves a job of any stage to closing and starts its wait now. A twin runner going, a left runner's worker
    log and a restart each start it over.
  - `wait` and `closed` read and end only a closing job.
- Which runner holds a job stays each occupant's `job`. `Engine::taken` derives the set where it is read. Nothing
  keeps it across an await.
- Build cache uploads that wait for GitHub stay keyed by runner (`ended_uploads`). They can outlive the job's
  closing.
- `engine/claims.rs` seals `Claims` by repository. `spend`, `consume`, `listing_answered` and `forget_unlisted`
  write it. A repository's claims still end once neither the config nor a drain lists it.

Nothing of the ledger survives a restart. `recover` follows again the jobs the history holds queued, and closes those
whose runner's container is gone, their wait counted from the restart. A reload keeps every job, those of a
repository it removed too.

## Consequences

A job can no longer be queued and closing at once, so the invariant that checked it is gone. The engine still reads
a job twice a tick while a twin runner runs it after its first runner went, once as running and once as closing.
That is unchanged here.
