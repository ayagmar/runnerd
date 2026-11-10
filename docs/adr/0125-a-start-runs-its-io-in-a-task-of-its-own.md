# 0125. A start runs its I/O in a task of its own

Status: **Accepted** (2026-11-10). Amends [0123](0123-a-slots-state-is-one-sealed-value.md): a slot is free,
starting or held. No wire, schema or config change.

## Context

A tick's plan starts its runners one at a time, and the engine awaits each start's GitHub registration, Docker start
and inspect inline. Starting several at once, which
[ADR 0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md) put off, needs a start's I/O to run
without the engine, and a state for a slot whose start is in flight that every budget counts. This decision makes
that split with one start in flight, so behaviour stays the same; letting more run at once is a later decision.

## Decision

- `engine/start.rs` holds one launcher for a tick's `apply` and a pass's `refill`: `start_batch` takes the plan's
  start decisions in order and returns what became of each. `apply` also skips starts while GitHub's rate limit is
  used up; `refill` checks it once, as it begins, as before.
- Each start is split in three:
  - **Launch, on the engine**, in today's order: the warm runner it replaces, `still_fits`, admission, the pool's
    labels, the slot's caches (`caches_ready`), the name, the mirror. It then builds the container's spec, which
    opens the runner's build cache socket, and marks the slot `Starting`.
  - **A task of its own** registers the runner (the JIT POST), deletes the registration if a stop was asked for
    meanwhile, starts the container and, only once it runs, inspects it. It decides nothing.
  - **The fold, on the engine**, ends the start: the slot is held by its runner, or by the container of a failed
    start that Docker kept, or free. Whatever leaves no container closes the build cache socket the launch opened:
    a failed or rate-limited registration, a stop's delete, and a Docker failure that kept nothing. The holds, the
    rate limit, `spend`, `recheck_image` and watching the runner (`watch_inspected`) are as before.
- The one reorder: the build cache socket opens before the registration, not after it. Only a start that fails
  before Docker sees it, and the fold closes it then.
- `CONCURRENT_STARTS = 1`.
- The batch awaits its tasks directly, never through `serving` or another `select!` that answers commands, runs
  turnover or notes found jobs: no command, plan, upkeep or publish sees a slot `Starting`. The `JoinSet` and the
  map from task to slot are the batch's locals, so a dropped batch aborts its tasks and leaves nothing on the
  engine. A task that panics panics the batch, as `serving` does; a cancelled one, which only happens as the runtime
  shuts down, is waited for, as in `serving`.
- `Slot` has a third state, `Starting`, with what the start reserves: its caps, its admission and when it was
  launched. The budget readers go through `Engine::footprints`, over held and starting slots, and `held_by` and
  `reserves_cpu` read a `Footprint` that a runner and a start both give. A start counts as the runner it becomes
  when just started: its cap under caps, and under measured admission its cap with no recent slice sample and its
  reservation otherwise. `plan_profile` reads `Slot::holder`, which tells all three states apart, and plans a
  `Starting` slot as a busy runner. `occupant()` and `occupant_mut()` read a `Starting` slot as no runner: in this
  decision only the budgets can see one.

Nothing of `Starting` survives a restart: a registration with no container is deleted by the first runners listing,
and a container is adopted or removed by recovery, as for a daemon that went in the middle of a start before.

## Consequences

Behaviour is unchanged: the invariants' trace is the same before and after, including generated failed and
rate-limited registrations, failed cache preparations and stops while a runner registers. Starting more at once is
then a change of `CONCURRENT_STARTS`, with what it needs besides: one JIT POST at a time, the readers other than
the budgets that would then see a `Starting` slot, and their tests.
