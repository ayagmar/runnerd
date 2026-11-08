# 0113. A released runner's slot does not wait for its registration's delete

Status: **Accepted** (2026-11-08). Amends [0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md): a released
runner's slot frees once Docker removed its container, without waiting for GitHub to delete its registration. No wire,
schema or config change.

## Context

A release removed the runner's container, then deleted its registration on GitHub, and only then freed the slot
(ADR 0095's order: log, failed job's record, history, removal, registration). GitHub drops an ephemeral runner's
registration with its job, so for a runner that ran one the delete answers "not found": over the 3 days before
0.0.30, 12,375 of 13,525 runners took a job, about 4,100 such deletes a day, each a GitHub round trip before the slot
took its next runner.

## Decision

- **The slot frees once Docker removed the container.** A registration with no container behind it takes no job,
  so ADR 0076's rule, which is about the container, holds.
- **A runner whose log shows its job's end (`completed with result:`) is not deleted.** Any other, one that took no
  job, was killed mid-job or left no log, is deleted in the background, as before but off the slot's path.
- **A stop waits for the deletes still in flight**, up to `STOP_DEADLINE` (30 s): the daemon's exit would drop them,
  leaving the registration on GitHub, offline, until runnerd starts again. Each tick forgets the finished ones.
- **The next runners listing deletes what either left**, as it does any registration of this machine with no
  container: one GitHub kept past its job, a delete that failed, or one a stop or restart cut short. For a runner
  released here (`ended_busy`) that delete logs nothing, nor does GitHub's `Busy`, as before.

## Consequences

- On the fakes with GitHub's deletes never answering, two runners that end while jobs are listed free their slots
  and both slots take a job within 5 s; before, neither did (`tests/pacing.rs`). The runner that logged its job's end
  gets no delete.
- A graceful stop with a delete GitHub holds waits for its answer, and one GitHub never answers ends the stop at its
  deadline (`tests/pacing.rs`).
- About 4,100 fewer GitHub requests a day on the owner's host.
- A registration GitHub keeps briefly after its job goes at the next runners listing instead, at most
  `poll_interval` later.
