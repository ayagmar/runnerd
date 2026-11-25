# 0180. A tick reads a job once, and not what its listing showed

Status: **Accepted** (2026-11-25). Amends [0109](0109-a-job-that-left-the-queue-is-followed-until-github-says-it-is-gone.md),
[0124](0124-running-jobs-are-read-every-poll-interval.md) and
[0085](0085-actions-cache-saves-wait-for-the-decision-not-the-job.md): a job read they ask for may be answered by what
the same tick's listing or an earlier read of the tick said. No wire, schema or config change.

## Context

A tick reads single jobs (`GET /repos/{repo}/actions/jobs/{id}`) for four reasons: queued jobs the listing no longer
shows (ADR 0109), runners' jobs admission cannot name yet, running jobs once a poll interval (ADR 0124), the jobs of
ended runners whose uploads wait (ADR 0085), and closing jobs of removed runners. Those reads were independent. A job
that was both an ended upload and closing was read twice in one tick, and both reads wrote its details. A queued job
whose read showed one of this machine's runners took it was read again for admission in the same tick. And the
listing that runs first in every tick reads each queued or in-progress run's jobs page
(`/actions/runs/{run}/jobs?filter=latest`), which describes every job of the run as fully as a job read, but kept only
the queued ones: the running job read seconds later was a second request for the same facts.

## Decision

- **A listing gives what it showed of the jobs followed.** `GithubApi::queued_jobs` takes the job IDs of the
  repository the engine follows (queued or closing in the ledger, named by a runner here, or an ended upload) and
  returns a `JobListing`: the queued jobs as before, and `observed`, the followed jobs its pages showed in another
  status. It makes no request more: no page, no run, and no workflow read for these jobs. An observed job is decoded
  as `JobDetails`, as a job read decodes it, and given only when it is the whole answer: of the run's latest attempt
  (the pages are read with `filter=latest`), a runner and a start once in progress, a conclusion and an end once
  completed, and its workflow named as a job read names it, from what the client already knows. A job of a run left
  unread, of an earlier attempt, or of a run whose workflow name would take a read is left out, as is every job of a
  listing that failed: the listing fails whole, as before.
- **One tick-local owner.** `Observed`, in `scheduler/engine/observed.rs`, holds what the tick's listings showed and
  each successful job read of the tick after them, a read replacing what was known. `list` makes it, `follow_jobs`
  and `follow_running` pass it along, and the tick drops it. Engine holds none of it: nothing of it survives the
  tick, a restart or a reload, and a repository not listed this tick has nothing in it.
- **What stands for a read.** For a queued job that left the queue, anything the tick said of it: the history's
  decision rests on its status and runner only, and absence from the listing still says only that it is not queued,
  so a job no answer showed is read, and its failed reads count as before. For a job a runner here took (its name for
  admission, a running read, an ended upload), only details that name that runner, which GitHub gives only once it
  took the job: a listing from before the take, or one that shows no runner yet, is read again. For a closing job,
  only that it completed, which nothing changes after; any other status is read.
- **Once a tick.** Running jobs and ended uploads are one set, each job asked once. A closing job the tick already
  saw completed closes without a read, and without its details written again when the tick recorded them.

## Consequences

- The cadence of ADR 0124 and the governor's pace (ADR 0127) are unchanged: a running job is described once a poll
  interval as before, from the listing when it showed the job's runner, otherwise from a read. Its steps may be the
  listing's, a few seconds older than a read after the starts would give.
- The saving needs the run to be listed: a repository listed this tick (ADR 0128), its run queued or in progress and
  among the runs read before `JOB_REQUESTS` ran out. A job whose run completed is read as before, and so is every job
  of a repository that was not due.
- The ledger's clocks of ADR 0109 count an observed answer as an answer, as a read's.
- An ended upload whose trust read failed is still read each tick its upload is held; only within the tick are its
  reads shared.
- A job read's kept answer, which a read of a completed job drops, stays in the conditional cache when the listing
  answered for the completion instead, until the cache's own bound evicts it.
- The invariants' fake GitHub shows every odd job in its run pages as its read describes it, so the generated
  sequences run with answers from listings and reads alike; the engine's tests in `tests/observed.rs` and
  `tests/build_cache.rs` check each rule above against reads.
- `Github`'s listing decodes each job item twice (the listing's fields, then a followed job's details), a cost in CPU
  only.
