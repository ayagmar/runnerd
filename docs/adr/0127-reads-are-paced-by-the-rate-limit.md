# 0127. GitHub reads are paced by what is left of the rate limit

Status: **Accepted** (2026-11-10). Amends [0124](0124-running-jobs-are-read-every-poll-interval.md) and
[0111](0111-a-tick-is-due-an-interval-after-the-last-one-started.md): the interval of the running jobs' reads and of
scheduled ticks is `poll_interval` times a factor the rate limit sets. No wire, schema or config change.

## Context

A token has 5,000 requests an hour, and other `gh` users share it. Until now the client read the limit only to see
whether it was 0 ([0112](0112-no-start-and-no-hold-while-githubs-rate-limit-is-used-up.md)): by then every start waits
until the reset. Registrations and deletes are what a job needs; the listings and job reads only find out about it, and
can come less often for a while. The request counts (PR 1) give the drop in `remaining` and the registrations and
deletes sent.

## Decision

- **A governor in `engine/governor.rs` keeps one pace step**, from `Steady` (`poll_interval` for everything) through
  running-job reads at 2× and 4×, active repositories' listings at `poll_interval` (no change until listings have a
  pace of their own; the step is kept so that change needs none here), to every listing at 2× and 4×. It owns the
  readings of the last ten minutes and the step, which is the newest reading's, in one value in a leaf file with
  private fields. The window reaches back to the last reading at or before ten minutes ago, so ticks further apart
  than that (4× a `poll_interval` over 2½ minutes) still judge the pace from the previous tick.
- **Each tick takes one reading** of the `core` limit the last answer carried (`GithubApi::spend`, with the count of
  registrations and deletes sent since the client was made) and moves the step at most one:
  - the **projected spend** is the drop in `remaining` since the step last changed, at most ten minutes, less the
    registrations and deletes in that drop, as a rate over the time to the reset. Other clients' requests are in the
    drop;
  - the **reserve** is the registrations-and-deletes rate over the whole ten minutes, over the time to the reset, and at
    least 100; the **room** is `remaining` less the reserve. The reserve is subtracted once: registrations and deletes
    are left out of the projected spend, as the reserve already stands for them;
  - **over the room: one step slower. Spare: one step faster**, where spare is the projected spend, multiplied by what
    undoing the step costs at most (2; 3 for listings at a third of the interval), within 90% of the room;
  - a pace is judged only after two minutes of readings under it, and from the readings since it began, so a step
    does not wait out the ten minutes it was meant to answer, and the one after it is not taken on the spend it
    already cut.
- **What the governor changes:** the running-job reads' interval (`RunningReads` due test) and the interval of
  scheduled ticks, which list every repository. A tick a command or a wake-up calls early still lists. Nothing that
  registers or deletes a runner asks the governor, so no registration or delete waits for it: one a start decides
  goes out in the tick that lists its job whatever the step. What a scheduled tick decides comes with the slower
  ticks: a job is seen up to 4× `poll_interval` later, an idle runner past its time is deleted and a warm runner's
  replacement registered at the next tick, not within `poll_interval`.
- **A new count of the limit** (a different reset time) clears the readings and returns to `Steady`: the new hour has
  all its requests. A reading whose reset has passed, or with no limit, changes nothing.
- **At 0 remaining, or on a secondary limit, [0112](0112-no-start-and-no-hold-while-githubs-rate-limit-is-used-up.md)
  applies unchanged.**
- **A change of step logs one line**, naming the pace before and after, what is left and the projection. No status or
  wire field.

## What survives a restart and a reload

Nothing survives a restart: a new engine starts at `Steady` and learns from the answers it gets. That is safe because
`Steady` is the pace before the governor, a step is judged after two minutes of readings, and the hour's budget is
not spent in two minutes at that pace; the 0112 path stops the starts if it is. A reload keeps the readings and the
step, and takes the new `poll_interval` from the next tick.

## Why the undo has a margin of its own

A step that was taken because the spend did not fit, undone as soon as it fits with 10% to spare, brings the spend
back up to what the step saved, which can be as much as double: the next reading is over the room and the step comes
back, every few minutes. So an undo counts what the step saved as spent again. A test holds the demand constant for an
hour over generated plants and asserts that no step is taken after one was undone.

## Consequences

- At a healthy limit nothing changes: the engine's calls are the same in the same order (a pacing test compares them).
- Easing is slower than slowing: the first step undone waits until the last ten minutes' average fits, and the others
  two minutes each.
- A burst of registrations raises the reserve, and so slows the reads, for ten minutes after it.
- Another client that drains the limit is slowed against, not stopped: runnerd's reads drop to a quarter and its
  registrations go on.
