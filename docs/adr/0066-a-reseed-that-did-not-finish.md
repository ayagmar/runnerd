# 0066. A reseed that did not finish starts the profile over from the image

Status: **Accepted** (2026-10-25). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md).

## Context

A slot's changes are an overlay upper made against the profile's snapshot. `image/seed.sh` replaces the snapshot
layer by layer, then drops every slot's changes, or drops both when it starts over. A seed container killed in
between, by an OOM kill, `docker kill` or a host reboot, left slots' changes over a snapshot that was half removed
or already replaced. A changed cache index could then name files the snapshot no longer held, the breakage of
[#113](https://github.com/ayagmar/runnerd/issues/113), and nothing noticed: the daemon only logs a failed reseed
([#115](https://github.com/ayagmar/runnerd/issues/115)).

## Decision

- `seed.sh` writes `/base/.reseeding` (`RESEED_MARKER` in `image/layers.sh`) before its first change to the
  snapshot or a slot's layers, and removes it after the last. A sync of the snapshot's and the slots' volumes comes
  after writing it, before removing it and after, so a host that crashes keeps the marker for as long as any change
  may be missing from disk. The copy of slot 1's view it publishes is made before it; a run that never got past
  copying changed nothing, and the next one deletes its copy.
- A seed run that finds the marker starts over from the image, whatever its threshold or limit, and removes it.
- A runner container that finds the marker leaves the snapshot out of every layer and drops its slot's own
  changes, so its job runs on the image alone. The daemon is unchanged: the profile reseeds once it is idle after
  slot 1 ran a job, as before, and that run clears the marker.
- The copies `seed.sh` and `mount_layers` clean, a snapshot's and a slot's changes, are never deleted through a
  symlink a job left: a `gradle` or `cargo` directory that is one goes instead of its daemon logs or secrets.

## Consequences

The jobs between a killed reseed and the next one run cold. The marker is no lock: a seed container still runs only
while no runner of the profile does. An image older than the change ignores the marker; going back to one after a
reseed that did not finish needs the profile's snapshot and slot volumes removed first.
