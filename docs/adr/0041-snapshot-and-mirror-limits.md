# 0041. A profile's snapshot and the mirror's storage have size limits

Status: **Accepted** (2026-10-23). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md).

## Context

A reseed folds slot 1's layer changes into the profile's snapshot, and nothing ever leaves it. On the owner's host
the node snapshot went from 3.2 GB to 11 GB in a day: 4.9 GB of it Flutter, two versions of 2.5 GB each,
2.2 GB of NDK and 2.4 GB of Gradle caches. Its Docker images are not in it: a slot's Docker Engine keeps them in the
slot's volume, which `DOCKER_BUDGET_GB` bounds. A slot's layer starts over above `LAYER_BUDGET_GB`, but the
snapshot had no limit. The Docker Hub mirror expires content after its TTL, with no limit on its size.

## Decision

- `[caches] snapshot_max` (default `10G`) bounds each profile's snapshot. A reseed measures the snapshot plus slot
  1's changes, an upper bound on the new snapshot; above the limit the snapshot starts over from slot 1's changes
  alone, which drops whatever no job changed since the last reseed, such as a superseded tool version. When slot 1's
  changes alone are over the limit, the snapshot starts over from the image and slot 1's changes are dropped as
  every other slot's are. `image/seed.sh` does this, given the limit as its second argument; a seed script of an
  older image ignores it.
- The default is above the 8.7 GB the owner's Flutter and Android profile needs once one Flutter version is gone, so
  it starts over when stale versions pile up, not after every reseed.
- `[caches] mirror.max_size` (default `10G`) bounds the mirror's storage as `max_size` bounds pnpm's and Maven's: when
  no runner runs and the mirror is down, a larger volume is removed, and the mirror starts empty with the next
  runner.
- `GET /v1/caches` reports both limits in each volume's existing `limit`, which `runnerctl caches` and the
  dashboard's Caches page already show.

## Consequences

A profile's jobs run cold once after its snapshot starts over, for the tools slot 1's last job did not change; the
next reseeds fold them back in. A snapshot already over the limit stays until its next reseed. A mirror emptied
pulls from Docker Hub again.
