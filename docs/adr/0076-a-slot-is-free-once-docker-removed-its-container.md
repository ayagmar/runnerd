# 0076. A slot is free once Docker removed its container

Status: **Accepted** (2026-10-30), amended by 0092.

## Context

Three ways the engine lost track of a runner when a Docker call failed:

- A start whose request failed after Docker made the container (a lost answer, a timeout, an upload or start that
  failed and whose removal failed too) dropped the container's ID. The slot looked free, so the next tick could
  start a second runner on the same slot volumes, and the error path deleted the registration of a runner that may
  already have taken a job.
- The starts of one tick were admitted on the plan's snapshot. Each start takes seconds (cache volumes, JIT
  registration, create and start), and the sampler reads every 2 s: a runner's memory rising, the host's memory
  falling or the slice reaching its high-water mark in between did not stop the later starts.
- A log capture that Docker failed to start, or that broke off while its container ran, was never started again.
  The job lost its name, log and record, and once completed it was recorded as another machine's.

## Decision

- A slot is free only once Docker confirmed its container's removal. `DockerApi::start` returns the container a
  failed start left (`StartError::container`): the ID once Docker gave one, found by the start's unique name when a
  create may have made one, the name when Docker cannot say. The engine keeps such a container in its slot as the
  slot's runner, reserving what it was admitted at, and leaves its registration and build cache with it.
  `collect_finished` removes it once it stopped, as any runner's; a running one is a runner and is watched as one.
  A restart adopts or removes it by its labels, as any runner container.
- Each start is admitted again right before it registers, by the plan's rule (`scheduler::Left::fits` over the
  budgets `plan` would have now): what runs, the latest samples, the host's latest reading with the margin the plan
  chose, the high-water mark, and the CPU budget. A start that no longer fits waits for the next tick, and so do the
  tick's later starts.
- A log capture that failed or broke off while its container runs starts again: at the next tick the first time,
  then after 5 s, doubling up to 5 minutes. It resumes from the later of where its own stored lines end and where
  the store says they end. When GitHub says a job left the queue for one of this machine's runners whose capture
  named no job, that runner claims it.

## Consequences

- A container a failed start left holds its slot, and its memory stays counted, until Docker removes it; the
  status shows it as the slot's runner. Its repository's pool is held back as after any failed start. If that hold
  ran out before Docker removed the container, its removal holds the pool back again, as a runner that ended without
  a job does, so the next hold of a streak comes one step sooner.
- A start whose own container's removal succeeded after the start failed has no container left, even if its
  runner had taken a job in between: that job fails as one whose runner vanished.
- A tick can start fewer runners than its plan when something changed while it started them; the queued jobs say
  why they wait at the next plan.
- A job that started and completed between two ticks while its capture was broken, or that no listing saw queued,
  is still recorded without its runner.
