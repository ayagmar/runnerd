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
  first reason that held it, in the order it checks them. The engine keeps the last plan's reasons, and
  `GET /v1/status` gives each job in `queued` an optional `waiting` (`api::Waiting`):
  `paused`, `backoff` (until when), `runner-starting`, `reseeding` and `pulling-image` (of which profile),
  `no-slot` (of which profile), `memory` and `cpu` (what the runner would reserve and what is left), and
  `behind-cpu` (the older job starved of CPU that goes first).
- A job a runner was started for, or that an idle runner will take, is `runner-starting`: GitHub hands a runner any
  job its labels match, so the reason is about the repository's next job, not this one in particular.
- The planner now checks for a free slot before the memory budget. Both only skip the job, so its decisions are the
  same; the reason it gives is the one the owner can act on first.
- `waiting` is absent before the first plan with the job, while the daemon shuts down, and from an older daemon;
  clients show nothing then rather than guess.

## Consequences

- The reasons are as fresh as the last tick. No event carries them: a client that keeps the status from events (the
  overview reads the status again at least every 10 s while samples arrive) sees a reason change late by that much.
- The planner's tests assert the reasons beside the decisions, so a change to the order of its checks shows up there.
- Rejected: an event per reason change (noise on every tick of a long queue), and working the reason out in each
  client from the budget and slots (wrong as soon as admission measures, and duplicated in three clients).
