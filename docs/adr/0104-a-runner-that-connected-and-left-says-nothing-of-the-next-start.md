# 0104. A runner that connected and left says nothing of the next start

Status: **Accepted** (2026-11-07). Amends [0050](0050-warm-runners.md) and
[0098](0098-a-pool-waits-longer-for-its-first-barrier.md). No wire, schema or config change.

## Context

A runner that ended without a job held its pool for 30 s, unless it had waited `IDLE_TIMEOUT`. The live host logged 27 such
exits of fretboard's light runners in 24 hours, and the 0.0.29 sandbox run logged them right after queued jobs were cancelled.
Under 0098 a backoff also clears the pool's waits and its served mark, so one cancelled job could put a served pool back on
600 s.

`actions/runner` explains the link. At v2.338.1, the release the image pins, `Runner.cs` acknowledges a job message before it
asks for the job. An ephemeral runner, which every JIT runner is, exits 0 if that acknowledgement says the job is gone, and it
only traces why. So its log ends `Listening for Jobs`, then `run-helper.sh`'s `Runner listener exit with 0 return code`. That
the live exits took this path is an inference: their containers are gone, and GitHub named no job for the ones checked. The
rule below does not rest on it, as a runner whose registration GitHub removed connected too.

The sandbox also showed two such exits 0.4 s apart, in one pool. A cancelled run takes several runners at once.

## Decision

- **`Listening for Jobs` says a runner connected.** The runner prints it once GitHub gave it a session. A runner that fails to
  start never gets there: a bad registration, a session conflict, a crash at start. As before, one that waited `IDLE_TIMEOUT`
  counts as connected too.
- **A runner that connected and left brings no hold on its own.** The pool's waits and served mark stay, and its next runner
  starts at the next plan. It takes the path 0050 gave a warm runner that went once.
- **The second in a row is a fault, and holds the pool as `HoldCause::IdleExits`.** A runner that starts, connects and leaves
  again and again would otherwise cost a registration every few seconds. A runner that takes a job ends the streak.
- **Runners already up when the last one left count with it.** `PoolHold::left_beside` names the pool's other runners at that
  exit, so a cancelled run counts once, however many runners it takes. Names, not times: nothing here reads a clock.
- **A runner that never connected holds its pool on its first exit,** and a pool whose runners keep failing to start backs off
  as it did. `exits_hold_only_what_failed_to_start` asserts both over the generated engine sequences.

## Consequences

- A cancelled job costs its pool nothing past the one runner.
- Two jobs cancelled one after the other, with no job taken between, still hold the pool, as the second runner started after
  the first left. That reads the same as the fault, and costs 30 s.
- A pool that connects and leaves backs off one generation later than one that fails to start.
- The streak lives in memory, like holds, so a restart forgets it.
- The warning stays for each such exit, with what the runner last said, and no `runner.failed` is sent until a hold.
