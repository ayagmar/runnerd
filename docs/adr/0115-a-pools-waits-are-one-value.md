# 0115. A pool's waits are one value, and a replay's state is sealed

Status: **Accepted** (2026-11-09). No wire, schema or config change.

## Context

`priority::Waits` had public fields, and five places wrote them: the plan's bookkeeping, a start, a listing, a reload
and a replay. Beside it, `RepositoryState.cpu_held` was a set of profiles with the same key as the `waits` map, written
in the same function and pruned by the same predicate. `Replayer`, the working state of one replay, sat in a file with child
modules, so its fields had no privacy boundary.

## Decision

- **`Waits` is sealed in `scheduler/waits.rs`, a leaf of its own.** Its fields are private and only its methods write them. The method
  set stays small and named for what happened (`note`, `retain`, `own`, `started`, `stopped`, `emptied`): `note` keeps
  answering whether its clock started, and `retain` keeps taking a closure, as a single `on(event)` would only have
  wrapped them.
- **`cpu_held` moves into `Waits`, and a start or a listing leaves it.** The status reads it between plans, and an
  `apply_start` in the middle of a tick is not a new plan: only the next plan's `set_cpu_held` says otherwise. It is
  set before the check that drops a pool with nothing kept. A pool whose last plan held it for CPU has a CPU wait, so
  that check gives the same answer it did with the separate set.
- **The two clocks stay.** A pool's next job for memory and its next job for CPU can be different jobs, so one clock
  would give one of them the other's age.
- **A replay goes through the same methods**, and never sets `cpu_held`: no status reads a replay.
- **`Replayer` moves to `admission/replay.rs`, a leaf, with its fields private.** That is its disposition for all of
  them: they are one replay's working state and never outlive the call to `simulate`, so splitting them would add
  types for no state that is ever seen from outside. `simulate` and the types only a replay uses move with it.

## Consequences

Nothing outside `Waits` can write a clock, and `check-state.py` proves that for `Waits` and `Replayer` alike. Nothing of
a pool's waits survives a restart, and a reload keeps those of the pools still configured, with `cpu_held` among them
now that one map holds both. `Waits::cpu_held` is a fact of the last plan and not of the waits, so a pool may have it
with both clocks stopped until the next plan; `is_empty` counts it for that reason.
