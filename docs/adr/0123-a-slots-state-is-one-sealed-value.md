# 0123. A slot's state is one sealed value

Status: **Accepted** (2026-11-10). No wire, schema or config change.

## Context

The engine kept each profile's slots as `Vec<Option<Occupant>>`. Whether a slot was free or held was an `Option`
that `runners`, `recover`, `control`, `tick`, `caches`, `status`, `history` and `admission` all read with `is_none`,
`flatten` or `as_ref`, and that `runners` and `recover` wrote with `take` and `= Some(..)`. Starting runners
concurrently, which [ADR 0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md) put off, needs a
third state, a start in flight, that every one of those readers counts. With an `Option`, nothing makes a reader say
how.

## Decision

- `engine/slot.rs` holds `Slot`, one per slot of a profile, with a private enum: free, or held by an occupant (a
  runner, or the container of a failed start that Docker kept). Only its methods write it: `place` and `vacate`.
- Readers use `is_free`, `occupant` and `occupant_mut`. `occupant_mut` lends the occupant out to note what its
  runner is doing; it does not change whether the slot is held.
- The engine still owns the map of each profile's slots: it adds slots on a reload and drops free ones past the
  configured count, as before.

Nothing of it survives a restart: every slot starts free, and recovery places each runner container it adopts in a
free one. A reload keeps it.

## Consequences

Behaviour is unchanged: the invariants' trace is the same before and after. A third state is one new variant of the
private enum. Each of `Slot`'s readers then has to answer for it in `slot.rs`, rather than at each call site.
