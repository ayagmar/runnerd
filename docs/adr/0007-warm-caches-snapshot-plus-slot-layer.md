# 0007. Warm caches: per-profile snapshot plus per-slot layer

Amended by [0041](0041-snapshot-and-mirror-limits.md): a snapshot over `[caches] snapshot_max` starts over at its
next reseed, from the image since [0065](0065-a-snapshot-starts-over-from-the-image.md), and the mirror's storage is
emptied above `mirror.max_size`. Amended by [0052](0052-shared-build-cache.md): a build cache the daemon keeps per
repository is shared by every slot, written only by the default branch's pushes, schedules and manual runs. Amended
by [0054](0054-slot-docker-stores-share-the-disk.md): the slots' Docker stores share a quarter of the disk not
otherwise used. Amended by [0066](0066-a-reseed-that-did-not-finish.md): a reseed that did not finish starts the
profile over from the image, and containers use neither its snapshot nor their slot's changes until then.

## Context

Fresh containers per job would re-download dependencies and images every time.

## Decision

Each profile has a read-only snapshot volume and each slot a writable layer volume. A pnpm store, a Maven repository and a Docker Hub pull-through mirror are shared (`crates/core/src/cache.rs`). When a profile is idle, slot 1's layer becomes the new snapshot. This is the model the bash `ci` tool proved.

## Consequences

Jobs start warm. Volumes grow, so shared caches are emptied above their limits while no job runs. A volume is never touched while a container uses it.
