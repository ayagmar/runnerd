# 0126. Up to four starts at once, one registration at a time

Status: **Accepted** (2026-11-10). Amends [0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md)
and [0095](0095-dockers-events-wake-a-pass-that-refills-a-slot.md): starts no longer go one at a time. Amends
[0125](0125-a-start-runs-its-io-in-a-task-of-its-own.md): more than one start is in flight, a pass stops launching on
the rate limit as a tick does, and readers besides the budgets see a start in flight. No wire, schema, config or
name-format change.

## Context

ADR 0092 kept starts one at a time because the waits then were removals and deferrals. Both have left the start path
since (0092, 0095, 0111), and what is left between a job's listing and its container is mostly the queue of starts
itself: each start's registration and container start, about 1.3 s on the fakes, before the next begins. A burst of
12 jobs into free slots got its 12th container 15.5 s after the listing that showed them on the fakes, and about 15 s
on the owner's host. ADR 0125 split a start into a launch on the engine, a task for its I/O
and a fold, at one start in flight.

GitHub asks clients to make requests one after another, not concurrently, and to leave a second between mutating
ones; a secondary rate limit, once hit, stops every request until it lifts (ADR 0112), which costs more than the
queue of starts does.

## Decision

- **Up to four starts of a batch are in flight** (`CONCURRENT_STARTS = 4`). Their Docker work, the container's start
  and its inspect, overlaps.
- **One registration at a time** (`POST_LANE = 1`): a semaphore of the batch, which a start's task holds for its JIT
  POST only. Registrations go one after another, back to back, about two a second on the owner's host: within the
  documented limits (80 content-creating requests a minute). Should the e2e see a secondary-limit answer, the lane
  spaces them a second apart, which puts the 12th container of a burst at about 12 s on the owner's host. A wider lane
  is a later decision, on the e2e's figures. Release deletes stay as concurrent as before (ADR 0113).
- **Each launch goes by every start that came back.** Before each launch, the batch folds every task that has ended,
  then checks, in order: a stop asked for, GitHub's rate limit used up, a failure that holds the start's pool back
  since the plan, and an earlier start of the batch deferred. A launch then waits for its slot's caches, the
  history's write of its name and the mirror, during which more tasks end; so once it is readied, the batch folds
  again and checks the stop, the limit and the hold again, before the start opens its build cache and its slot
  becomes `Starting`. That last step waits for nothing, so nothing comes back between the batch's last look and the
  start. One gate for a tick's starts and a pass's: a pass now
  stops launching once a registration is refused for the rate limit, as a tick did.
  - A pool held mid-batch launches nothing more; its starts in flight finish. The start is skipped, not deferred: other
    pools' starts go on, no replan follows, and in a pass its job keeps its waits. A hold during a hold is no hold
    (`PoolHealth::hold`), so failures of one batch do not lengthen the backoff.
  - A registration refused for the rate limit is carried as skipped, as a start the limit kept from launching is: its
    job keeps its waits. The client then fails every request at once until the limit resets, so the starts waiting for
    their turn to register fail at once too, and their build cache sockets close. No pool is held (ADR 0112).
  - Once a start is deferred, the plan's later starts wait for the next plan, the starts in flight finish, and the
    tick plans again once the batch is done (ADR 0092).
- **A task checks the stop once it holds the lane**, before its POST, and sends nothing if one was asked for. A stop
  therefore costs no registration and delete for each start waiting for its turn. One GitHub registered as the stop
  came is deleted before its container starts, as before. A stop that comes during a launch's own waits leaves its slot free
  and opens no build cache, as the batch checks again once the launch is readied.
- **A failed start's registration is deleted in the background** (`abandon`, into ADR 0113's `deletes`): the fold no
  longer waits for GitHub, so one slow DELETE holds back no other start of the batch. A stop waits for it up to
  `STOP_DEADLINE`, and the next runners listing deletes one it left.
- **Preparing a slot's caches stays at launch**, on the engine, before the name and the build cache: a failed prepare
  sends no registration and never makes the slot `Starting`. It is serial Docker work for each launch, as is the
  durable write of the runner's name to the history, which a restart names later than. Both overlap the batch's
  starts in flight.
- **What reads a start in flight**, as a batch runs:
  - the budgets, through `Engine::footprints` (ADR 0125), and `plan_budget`'s choice of the host margin, which counts a
    start in flight as something running in the slice. That one changes nothing today: no plan runs mid-batch, and
    `still_fits` keeps the margin its plan chose;
  - `warm_purpose`, which counts a start in flight of the pool as a runner with no job yet, as the runner it becomes
    is once placed, so a batch of warm starts records the configured warm runner once and the rest as `burst`, as one
    at a time did;
  - `holds_runner`, which counts the slot as held, and `taken`, which finds no job named.

  No other reader runs between a launch and the batch's end: plans, upkeep, commands and the status wait for it.
- **Names may run further ahead of the clock.** Each name still carries a second later than the last, so a batch
  names its starts a second apart, and sustained bursts at about two launches a second take the counter ahead of the
  clock by up to a second for every second of load. A restart takes the counter from the names its containers and
  its history hold, as before.

Nothing of a start in flight survives a restart (ADR 0125): a daemon that goes in the middle of a batch can leave up
to four registrations with no container, which the next start's first runners listing deletes, and containers no
slot holds, which recovery adopts or removes.

## Consequences

- On the fakes (a registration 0.3 s, a container's start 1 s), a burst of 12 jobs into free slots gets its containers
  at 1.2, 1.5, 1.8, 2.1, 2.5, 2.8, 3.1, 3.4, 3.8, 4.1, 4.4 and 4.7 s after the listing, against 1.3 s a place before
  (15.5 s for the 12th). Four registrations in flight would bring the 12th to 3.9 s: the lane costs 0.8 s of it. The
  expected figure on the owner's host is about 7 s for the 12th, and a median from listing to container of about 6 s
  against 9.4 s; the e2e measures it.
- A container start that hangs holds back no other start of its batch, but the batch still ends with it, and the tick,
  passes, commands and status wait as long, as one start did before.
- Docker's starts may slow one another under sysbox. The fakes' Docker has a term for that, zero unless a test sets it;
  `workload_figures` prints a burst under a placeholder value on a line of its own until the e2e measures it.
