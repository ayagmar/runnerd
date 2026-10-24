# 0044. Queued jobs say why they wait

Status: **Accepted** (2026-10-24).

## Context

The owner's pain is waiting: ten slots, jobs queued in bursts, and measured admission and auto sizing deciding what
starts. The status listed each queued job but not why it had no runner, so the dashboard showed a job waiting beside
a slot that looked free, and the owner had to work out from the budget, the slots and the logs whether memory, CPU,
a pause, a reseed or a pull held it. A client cannot work it out either: it would have to repeat `scheduler::plan`
with numbers (predicted peaks, the high-water mark, CPU priority) the status does not carry.

## Decision

- `scheduler::plan` returns a `Plan`: its decisions, and for each matching queued job it starts no runner for, the
  first reason that held it, in the order it checks them. Each status the engine publishes plans again for the
  reasons, so they describe the engine as it is then (after this tick's starts and stops, a failed start's backoff,
  a pause or resume a moment ago), and `GET /v1/status` gives each job in `queued` an optional `waiting` (`api::Waiting`):
  `paused`, `backoff` (until when), `runner-starting`, `reseeding` and `pulling-image` (of which profile),
  `no-slot` (of which profile), `memory` and `cpu` (what the runner would reserve and what is left), and
  `behind-cpu` (the older job starved of CPU that goes first), and `behind-start` (the older job a runner starts for
  first, which takes the slot, memory or CPU this one lacks).
- A job that an idle runner will take is `runner-starting`: GitHub hands a runner any job its labels match, so the
  reason is about the repository's next job, not this one in particular. A job the plan would start a runner for has
  no reason yet: that runner does not exist until the start succeeds.
- The planner now checks for a free slot before the memory budget. Both only skip the job, so its decisions are the
  same; the reason it gives is the one the owner can act on first.
- `waiting` is absent before the first plan with the job, while the daemon shuts down, and from an older daemon;
  clients show nothing then rather than guess.

## Consequences

- The reasons are as fresh as the last publish: every tick and every command. Planning again on each publish costs
  the planner's pure pass plus a few store reads for measured admission, against a tick that already plans once.
- Amended (2026-10-24, round 1 review): the engine first kept the tick's plan, made before its starts ran, so a failed
  start still said `runner-starting`, a runner stopped over budget left its job claiming it, and a resume kept
  `paused` until the next tick. No event carries them: a client that keeps the status from events (the
  overview reads the status again at least every 10 s while samples arrive) sees a reason change late by that much.
- The planner's tests assert the reasons beside the decisions, so a change to the order of its checks shows up there.
- Rejected: an event per reason change (noise on every tick of a long queue), and working the reason out in each
  client from the budget and slots (wrong as soon as admission measures, and duplicated in three clients).
- Amended again (2026-10-24, round 2 review): planning for the status alone takes slots, memory and CPU for starts
  it never runs, so a younger job said "all slots busy" beside a free slot, right after a resume or after a slot's
  caches could not be prepared. A job held only by the plan's own starts now says `behind-start`; `no-slot`,
  `memory` and `cpu` mean what runs now. The plan's decisions are unchanged.
- Amended (2026-10-24, the TUI): the reason's words are `api::Waiting::reason` in core, so the dashboard and
  `runnerctl tui` say the same thing (`Waiting::kind` says how to draw it). The TUI keeps the status from events, so, as
  the dashboard does, it reads `GET /v1/status` again after each job and slot event and every 10 s while samples
  arrive, one read at a time, and takes only the reasons from it: the queue itself stays the one events keep. A job or slot event
  clears every reason until a read made after it answers, and a read such an event overtook is dropped and made
  again, so no reason from before the change shows.
