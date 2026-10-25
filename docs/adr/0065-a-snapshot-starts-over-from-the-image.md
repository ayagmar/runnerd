# 0065. A snapshot over its limit starts over from the image

Status: **Accepted** (2026-10-25). Amends [0041](0041-snapshot-and-mirror-limits.md).

## Context

ADR 0041 started a snapshot over its `snapshot_max` from slot 1's changes alone. Those changes hold the files slot
1's jobs wrote, not the ones they only read from the snapshot, so the new snapshot was not whole: a cache index slot
1 updated named content it had only read, which was gone. On the owner's host every Gradle and npm job of the jvm
profile failed after such a reseed, with Gradle's worker classes and npm's cached content missing, until the
profile's volumes were removed ([#113](https://github.com/ayagmar/runnerd/issues/113)).

The limit was also checked against the old snapshot plus slot 1's changes, an upper bound that counts a file slot 1
changed twice, so a reseed could start over for a snapshot that would have fit.

## Decision

- A reseed with a limit measures the snapshot it would make: slot 1's view of each layer it changed, its changes
  over the snapshot, and the snapshot of each other layer.
- A snapshot that fits replaces the old one, as below the limit.
- One that does not starts over from the image, with every slot's changes dropped, as 0041 did when slot 1's
  changes alone were over the limit. The image is the only start that is whole on its own.
- Pruning the merged view instead would need an eviction rule each cache keeps whole, such as Gradle's own cleanup
  or `npm cache verify`; runnerd has none, so it does not prune.

## Consequences

Every slot of a profile whose snapshot starts over runs its next job cold, not only for what slot 1's last job did
not touch; the reseeds after fold its caches back in. A snapshot starts over less often, as a file slot 1 changed is
counted once. The measurement walks slot 1's view of each layer, which costs a reseed with a limit a few seconds
more. An older image's seed script keeps 0041's rule.
