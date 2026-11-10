# 0121. A repository keeps its pools in one map

Status: **Accepted** (2026-11-10). No wire, schema or config change.

## Context

`RepositoryState` kept, beside a repository's listing, two maps keyed by the profile of each of its pools: `holds`,
each pool's health ([ADR 0058](0058-a-repository-served-by-several-profiles.md)), and `waits`, what each pool's next
job waits for ([ADR 0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md),
[ADR 0098](0098-a-pool-waits-longer-for-its-first-barrier.md)). Its fields were written from `runners`, `admission`,
`tick` and `control`, and it lived in `engine.rs`, which has child modules, so nothing kept those writes to one owner.

## Decision

- `engine/repository_state.rs` holds `RepositoryState`, with private fields. Only its methods and those of its
  `PoolState` write them.
- The two maps are one, `pools`, of `PoolState { health: Option<PoolHealth>, waits: Option<Waits> }`. Each half
  keeps its own lifetime:
  - The health is made by the first runner of the pool that leaves or fails. It goes only when a reload changes the
    pool's runners.
  - The waits go when the pool waits for nothing, and when a reload removes the pool. The health stays.
  - A pool's entry goes once both halves are gone.
- Readers of every pool's waits see only the pools that wait, as before.
- The listing stays its own sealed value. `RepositoryState` lends it out through `listing` and `listing_mut`.

Nothing of it survives a restart. A reload keeps it for a repository the config still lists.

## Consequences

The state rule's check finds nothing in `RepositoryState` any more. A third per-pool fact goes in `PoolState`, with
its own lifetime stated there, not in a third map.
