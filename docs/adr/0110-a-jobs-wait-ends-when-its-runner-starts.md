# 0110. A job's wait for a slot or the host ends when its runner starts

Status: **Accepted** (2026-11-08). Amends [0090](0090-slots-follow-waits-for-a-slot-alone-and-dockers-disk.md) and
[0103](0103-a-job-the-host-limit-holds-with-room-free-is-not-short-of-memory.md). No wire, schema or config change.

## Context

Auto sizing reads two waits per job: `slot_waited` (0090) and `host_waited` (0103). The engine kept the jobs waiting at
the last commit and added the time since to each of them at the next commit, and only a tick committed. Passes between
ticks (0095) start runners too, as do a tick's later plans, so a job that a pass started two seconds after a tick was
charged the whole tick as a wait. Over three days of live history, 40% of the jobs with a slot wait and 27% of those
with a host wait were charged more than their whole wait from listing to start, by about 15 s on average. Measured
auto sizing adds slots on the p95 of `slot_waited` and on how many jobs waited at once, and caps sizing subtracts
`host_waited` from the waits it reads as waits for the budget, so the excess skewed both.

## Decision

- A plan's start says which queued job it is for (`StartFor::Job`) or that it is a warm runner (`StartFor::Warm`).
- Once a job's runner is in its slot, its slot and host waits end: it is charged the time since the last commit and
  leaves both sets.
- When a capture or GitHub names the job a runner took, that job's waits end too: GitHub picks the job, which may not
  be the one the plan started the runner for.
- A job waits for a slot or for the host's memory, never both: joining one wait ends the other.
- Every plan carried out commits the slot waits: a tick's first plan and its later plans, and each pass. A pass also
  commits the host waits of its plan. The host's band, its log lines and what insights hear stay the tick's.

## Consequences

- A job's recorded slot and host waits no longer run past its runner's start. The engine's invariant harness checks
  this after every tick and pass.
- A job that starts to wait at a pass is counted from that pass, not from the next tick.
- A job whose planned start a pass does not make (it takes a warm runner's room, no longer fits, or a stop was asked
  for) stays in the wait it was in. On a tick, such a job leaves the slot wait until the next plan counts it, as
  below; the host wait is committed after the tick's starts.
- A start a tick planned and deferred, or one that fails before Docker made its container, no longer counts as a wait for a
  slot from that plan on: the plan gave it one. The next plan counts it again by what it then waits for.
