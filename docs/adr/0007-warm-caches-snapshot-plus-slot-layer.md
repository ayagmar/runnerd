# 0007. Warm caches: per-profile snapshot plus per-slot layer

Amended by [0041](0041-snapshot-and-mirror-limits.md): a snapshot over `[caches] snapshot_max` starts over from slot
1's changes at its next reseed, and the mirror's storage is emptied above `mirror.max_size`.

## Context

Fresh containers per job would re-download dependencies and images every time.

## Decision

Each profile has a read-only snapshot volume and each slot a writable layer volume. A pnpm store, a Maven repository and a Docker Hub pull-through mirror are shared (`crates/core/src/cache.rs`). When a profile is idle, slot 1's layer becomes the new snapshot. This is the model the bash `ci` tool proved.

## Consequences

Jobs start warm. Volumes grow, so shared caches are emptied above their limits while no job runs. A volume is never touched while a container uses it.
