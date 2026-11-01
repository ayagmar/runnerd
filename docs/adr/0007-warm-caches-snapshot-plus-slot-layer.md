# 0007. Warm caches: per-profile snapshot plus per-slot layer

Status: **Accepted**, amended by [0041](0041-snapshot-and-mirror-limits.md), [0052](0052-shared-build-cache.md), [0054](0054-slot-docker-stores-share-the-disk.md), [0066](0066-a-reseed-that-did-not-finish.md), [0067](0067-whiteouts-of-the-image-beside-the-snapshot.md), [0077](0077-cache-credentials-closed-slots-and-build-cache-counts.md), [0080](0080-a-job-starts-in-the-slot-that-ran-it-last.md).

## Context

Fresh containers per job would re-download dependencies and images every time.

## Decision

Each profile has a read-only snapshot volume and each slot a writable layer volume. A pnpm store, a Maven repository and a Docker Hub pull-through mirror are shared (`crates/core/src/cache.rs`). When a profile is idle, slot 1's layer becomes the new snapshot. This is the model the bash `ci` tool proved.

## Consequences

Jobs start warm. Volumes grow, so shared caches are emptied above their limits while no job runs. A volume is never touched while a container uses it.
