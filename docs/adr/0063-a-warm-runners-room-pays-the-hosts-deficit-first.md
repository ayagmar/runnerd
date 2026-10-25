# 0063. A warm runner's room pays the host's deficit first, and a seed container fits the budget

Status: **Accepted** (2026-10-25). Amends [0055](0055-admission-follows-the-hosts-memory.md).

## Context

ADR 0055 bounds new runners by what the host leaves the slice. Two starts escaped that bound. A job that stopped
another pool's warm runner for room was credited the stopped runner's reservation against a host spare clamped at
0, so on a host already past its limit it could start a runner the host had no room for. The slice's memory and
the CPU budget had the same flaw. And a reseed's seed container, which runs at its profile's memory cap, started
whenever its profile's slots were free, whatever the slice or the host had left.

## Decision

- **A warm runner's room pays the host's deficit first.** A job may stop warm runners of other pools for room, and
  each stop gives back what its runner reserves while the limit stays put: its memory moves from the slice to
  `MemAvailable`. So the host's spare is kept negative by what the running ones reserve past the limit, and a job
  takes their room only if it fits once that is paid back. The status still shows the spare as at least 0. The
  slice's memory and the CPU budget count the same way. Under measured admission the runners can take past
  `MemoryMax`, and a reload that turns on `cpu_overcommit` can leave them reserving past the CPU budget; a
  stopped runner's room pays that back before a start takes it.
- **A seed container fits the budget as a runner does.** It starts only where its cap fits in the slice's
  `MemoryMax` and in what the host leaves the slice, and not while measured admission holds the slice at its
  high-water mark. Until then the reseed stays due.
- **A requested reseed holds its room once its profile drained.** `runnerd reseed` stops the profile's jobs from
  starting. Once its slots are free, the seed's cap counts in the budget, so other profiles' starts leave that
  room and the seed starts as their runners finish.
- **A due reseed that is not asked for does not hold warm runners back for nothing.** The profile's warm runners
  stop for it only if its seed would fit once they stop.

## Consequences

- On a host past its limit, a job stops another pool's warm runner only if that runner's reservation covers the
  deficit and the new runner; otherwise it waits with `HostMemory`.
- A due reseed may wait a long time under steady load from other profiles. A requested one waits at most until
  enough of their runners finish, and meanwhile the budget shows its room as the seed's part.
- A requested reseed is not kept across a restart, as before: it is asked for again.
