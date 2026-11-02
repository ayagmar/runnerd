# 0095. Docker's events wake a pass that refills a slot whose runner ended

Status: **Accepted** (2026-11-02). Amends [0076](0076-a-slot-is-free-once-docker-removed-its-container.md): a pass
releases runners too, and the tick's inspect leaves those Docker said stopped to it. Amends
[0026](0026-commands-answered-while-a-tick-waits-for-github.md): passes run where commands are answered. Amends
[0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md): runners also start between polls, into
the room of a runner that ended.

## Context

On the owner's host after 0.0.26, with every slot of a pool busy and more of its jobs queued, a slot whose job ended
waited a median 37 s for its next runner's container to start. The jobs' records split it (journal up to
2026-11-02 15:00 UTC, each job counted once):

- **GitHub's job end to "ended" in the log: median 19 s.** Only the tick inspected runners, every `poll_interval`
  (15 s) after the last tick ended, so a runner that exited waited for the next one. Part of it is the image's own
  exit work after the job (the runner's exit, the entrypoint's wait of up to 2 s for it, `image/scrub.sh`); the
  journal cannot tell how much, since Docker kept too few `die` events to match them.
- **"ended" to its slot cleaned: 0.17 s.** Reading the log, recording a failed job and removing the container are not
  the cost.
- **Cleaned to the next container's start: median 13 s, p90 78 s.** The tick that freed the slot listed GitHub, followed
  its jobs and started the plan's runners one at a time before this one; a slot freed after the plan waited for the
  next tick.

Docker says when a container stops, as an event, at once. Its events are not a record: a stream that breaks loses
what happened meanwhile, and Docker keeps no history to ask again.

## Decision

- **The engine follows Docker's events as hints** (`DockerApi::exits`: `die` events of containers labelled
  `runnerd.job`). A task the engine owns (`engine/exits`) notes each container they name and wakes the engine; it
  decides nothing and starts nothing, and the engine aborts it when its loop ends, so it cannot outlive a stop. A
  stream that fails or ends is followed again after 1 s, doubling to 30 s while it keeps failing, and back to 1 s
  after one lasted a minute; the first failure of a streak logs a warning. Once a new stream lasted a second, the
  engine notes that any runner may have stopped meanwhile and inspects them all: that covers runners that stopped
  before it followed and while it could not. Past 64 containers noted at once it does the same.
- **A pass releases what Docker's inspect says stopped** (`Engine::turnover`), between ticks and while a tick waits
  for GitHub's listings and job reads, where commands are answered too. It inspects each noted runner: one still
  running stays, one that stopped is released as the tick's `collect_finished` releases it (its log read, an OOM kill
  and a failed job recorded, the container removed, then its registration), one Docker no longer has is gone. A slot
  is free only once Docker confirmed the removal (ADR 0076). An event names a container, never frees one; duplicates
  and events for containers no slot holds change nothing. While runnerd runs on, the tick's inspect skips the runners
  noted stopped, which the pass during its listing releases; the tick still inspects every other runner, so a
  runner whose event never came is found as before.
- **The pass then starts runners into the room it freed, from the last listing**, asking GitHub nothing more. It
  starts only runners for listed jobs into empty slots: no stops, no warm runners, no start that takes a warm
  runner's room; those wait for the tick. Each start is admitted as a tick's is, and the first that no longer fits
  ends the pass. No start begins once a stop is asked for, and a runner registered meanwhile is deleted before its
  container starts (ADR 0092).
- **No job gets a second runner from a listing older than its first** ([ADR 0018](0018-job-identity-and-logs-from-inside-the-runner.md)).
  The listing a pass plans from is a tick's, runners before queued jobs. Before it plans, the pass applies the jobs
  captures named since; it leaves out the jobs runners in slots run and those of runners released since (`closing`,
  until a listing stops showing them). A repository whose runner took a job no capture named, ended without one
  named, or whose start failed after registering it, plans nothing from that listing until the next poll. A runner a listing shows busy for the first time,
  whose capture has not named its job yet, counts against one of its pool's listed jobs; this also closed a twin a
  poll could start before, with a capture that failed. What a cached listing cannot know: a job cancelled or taken by
  another machine since it may get a runner. That runner waits as any idle runner does: it takes the next job of its
  labels, yields its room to a queued job, or stops after `IDLE_TIMEOUT`, as one a tick started just before the job
  went did.
- **GitHub is polled as before**: every `poll_interval` after a tick ends. A pass neither moves nor resets that.
- **The release keeps its order** (log, failed-job record, history, removal, registration). It takes 0.6 s in the
  fakes, against 1.3 s for a start; neither dominates once the exit is seen at once.
- **Starts stay one at a time** (ADR 0092): a pass's start is the next in line, not one in parallel.
- **A slot's runners never share a name**: each one's name carries a second later than the slot's last, as a slot can
  now start two within a second.
- **The "ended" line says how long after its container stopped**, from Docker's `FinishedAt`, so the journal shows
  the detection's share of a turnover.

## Consequences

- On the fakes with the owner's latencies (a GitHub request 0.3 s, a listing 3 s more, a container's start 1 s, a
  stopped container's removal 0.3 s), 16 jobs through 4 slots, a slot whose job ended started its next runner within
  4 s, at most 20 s while Docker's events were down (`tests/pacing.rs`). Six bursts of 40 jobs through 8 slots
  (`workload_figures`): a slot's job end to its next container start went from a median 24.5 s, p90 28.5 s, to
  1.9 s and 3.7 s (5.6 s at most), and a job's queueing to its container's start from a median 92.1 s, p90 177.7 s,
  to 58.8 s and 104.7 s. Jobs still wait for slots: 40 jobs outnumber 8 slots.
- A pass lists nothing on GitHub: it asks only for the registration and the deletion each runner needs anyway. Ticks
  got shorter, as passes start many of the runners, so the next poll comes sooner after one, never sooner than
  `poll_interval`: at 15 s, 240 listings an hour at most, as before. In those bursts, per hour: queued-jobs listings
  174 before, 210 after; runners listings 176 and 212; job reads 800 and 860; registrations 480 and 480; deletions
  480 and 484.
- A hint that comes while a tick removes or starts runners waits for that tick's end, about 1.3 s per start left.
- Docker's events down only slows turnover back to the poll's.
