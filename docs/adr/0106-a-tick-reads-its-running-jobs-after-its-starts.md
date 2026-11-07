# 0106. A tick reads its running jobs after its starts

Status: **Accepted** (2026-11-07). Amends [0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md): the
pace line counts the passes a tick hosts on their own. No wire, schema or config change.

## Context

On the owner's host, over the 24 hours before 0.0.29 (247 ticks that logged their pace, those longer than
`poll_interval`), the tick's "following" phase, between its listings and its plan, took a median 7.0 s, p95 21.4 s, more
than the listings themselves (median 3.8 s). Two things filled it:

- **Passes the tick hosted** while it waited for GitHub (ADR 0095): 860 s of the 2068 s, in 116 of the 247 ticks, during
  which they started 471 runners. That time went to starts, but the pace line called it following.
- **Reading every running job from GitHub before the plan.** The rest, median 4.4 s, p95 10.1 s, grew with the jobs
  running: a median 2.2 s with up to 4 running, 3.9 s with 5 to 9, 6.6 s with 10 to 14. A request takes about 1.9 s on
  the owner's host (the listings' median over two rounds), and `follow_jobs` read each running job's details, four at a
  time, so that a job listed in the tick waited half a second per running job before its runner started.

The plan needs only some of those details: a job admission cannot tell apart yet, as one a runner took before any listing
showed it, is named by them, which sets what its runner reserves and which run it belongs to. A job already named needs
them for its steps in the history and its build cache writes, neither of which a start waits for.

## Decision

- **Before the plan, a tick reads only the running jobs whose runners' jobs admission cannot name yet**
  (`follow_jobs`), with the jobs that left the queue, as before. **After its last start it reads the others**, and the
  jobs of ended runners whose build cache uploads wait, decides their writes and follows the jobs of removed runners
  (`follow_running`). Each running job is still read once a tick: one asked before the plan, answered or not, is not
  asked again after.
- **The pace line counts the passes a tick hosts as `passes`**, apart from the phase whose wait hosted them.

## Consequences

- On the fakes with GitHub at 1.8 s a request and a start at 2.8 s (its registration and its container), a job listed
  while 6 runners ran started 6.3 s after the listing that showed it, and 2.7 s now; with 12 running, 8.1 s and 2.7 s
  (`tests/pacing.rs`). GitHub is asked no more: in `workload_figures`, 772 job reads an hour before and 744 after.
- A running job's steps and its build cache decision come a tick's starts later than before, seconds at most.
- A job a runner took from a listing is named by the history's record of that listing, so its details are read after
  the plan from its first tick on; the listing already placed it in its run. Only a job no listing showed is read
  before the plan. After a restart no runner's job is named yet, so the first tick reads them all before its plan, as
  before.
- A stop asked during a tick's starts skips the reads after them: the stop's own tick reads the jobs, or the next
  start does, and its idle runners are removed sooner (ADR 0091).
