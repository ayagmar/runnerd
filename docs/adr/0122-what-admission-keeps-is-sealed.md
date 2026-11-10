# 0122. What admission keeps between plans is sealed

Status: **Accepted** (2026-11-10). No wire, schema or config change.

## Context

`Measured` holds what admission keeps from one plan to the next: the host band the last plan saw, the jobs waiting
for a slot or the host, the predictor and when it read the history, the runners `rereserve` raised, and three memos
that make a change log once (high water, the barrier, what the host's limit held). It lives in `engine/admission.rs`,
but `Engine` methods there and in `status.rs` and the tests wrote its fields directly.

## Decision

- Its fields are private, and only its methods write them. Each field keeps its own meaning and lifetime, and no
  two of them form one state.
- Each memo is replaced by a method that returns what it was, or whether it changed: `set_host`, `set_host_holds`,
  `set_high_water` and `set_barrier`. The caller decides what to log.
- `rereserve` still takes the raised runners out, updates them over the slots and gives them back. It now takes them
  before it borrows the predictor, and gives them back unchanged when there is no predictor, as before.
- Readers outside admission go through `host`, `high_water` and `waiting`.

Nothing of it survives a restart. A reload keeps it.

## Consequences

The state rule's check finds nothing left in the engine. The barrier and host-holds memos only decide log lines,
which no test reads, so a mutant there is visible in the logs alone.
