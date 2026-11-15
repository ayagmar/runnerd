# 0142. A runner's layers skip their unmount sync; its Docker Engine's cannot

Status: **Accepted** (2026-11-15). Amends [0141](0141-runners-answer-sync-calls-without-syncing.md).

## Context

ADR 0141's filter answers a runner's sync calls, and did not change how often btrfs commits. On the owner's host,
unmounting an overlay whose upper directory is on btrfs forces one transaction commit of the whole file system,
whatever wrote it: 150 mount, write and unmount cycles in a sysbox container on a Docker volume made 154 commits,
against 4 with no overlay. The kernel syncs the upper file system on unmount; a sync call filter cannot see it. With
overlayfs's `volatile` option the same cycles made 4: a volatile overlay syncs nothing, on unmount or when asked.

A runner mounts overlays at three levels: runnerd's layers of its slot (`mount_layers` in `image/layers.sh`), once
per runner; the containers of its Docker Engine, through containerd's overlayfs snapshotter; and BuildKit's steps,
in the engine and in a buildx `docker-container` builder. Seed containers mount only read-only overlays, which have
no upper directory to sync.

A volatile mount leaves `work/work/incompat/volatile` in its work directory, after an orderly unmount too, and
overlayfs then refuses to mount that upper and work directory again, volatile or not, until it is removed. The kernel
leaves the call to the user: removing it is right when the system has not crashed since.

## Decision

- runnerd's layers mount `volatile` when runnerd asks the runner to skip its syncs (`RUNNERD_SKIP_SYNCS=1`, `[host]
  skip_syncs`, ADR 0141). One switch: what a host crash costs is the same, the slot's volume, and the guard that
  empties it is the same. The variable and `runnerd.skip-syncs=1` stay as they are, so a daemon of 0.1.2 that asks
  an image of this change gets volatile layers under the same guard; the image's second label,
  `runnerd.volatile-layers=1` (`docker::VOLATILE_LAYERS_LABEL`), tells doctor what its runners skip.
- `mount_layer` removes a layer's `work/work/incompat` before mounting it, whatever runnerd asks now. That is safe
  because the daemon starts no runner on a slot's volume a crash may have torn: from the first runner that skips its
  syncs, the `unsynced` mark stays until a `sync(2)` once a stop leaves no runner running, and a boot that did not end
  in an orderly shutdown empties every slot's volume before any runner starts. So a runner finding the directory is
  on a volume whose last mount ended in an orderly unmount, a killed container's included, in this boot or one that
  synced. The exception is ADR 0141's: a volume another container holds for 5 ticks starts runners on what it holds.
- A kernel or file system that refuses `volatile` gets the plain mount, and the container's output says which layer
  and why. Only an error from the mount itself counts; the plain mount that follows fails the start as before.
- The slot's Docker Engine and BuildKit keep their overlays as they are. containerd 2.4.2's overlayfs snapshotter
  has a `mount_options` setting, applied to every overlay it returns, and an image could own containerd's config by
  starting containerd itself; but Docker 29.9.2 mounts a container's file system when it creates it and again when it
  starts it, and again for a restart, a `docker cp` or `docker start` after it exits. With `volatile` in the
  snapshotter's options the second mount fails, so no `docker run` starts (measured below). containerd's own
  temporary mounts and BuildKit's mounter strip the option for exactly this reason
  (`mount.RemoveVolatileOption`), so BuildKit could not use it either, and a buildx builder's `buildkitd.toml` has
  no mount options at all. Putting the whole Docker store under one volatile overlay does not work: overlayfs is not
  supported as an upper directory. Docker's legacy `overlay2` driver has no such option. What would reach them is
  in Docker: its mounter removing the directory before it mounts a container's file system again, as this image does
  for its layers.

## Old and new

- An image of this change and a daemon of 0.1.2: the daemon reads `runnerd.skip-syncs=1`, arms its guard and asks;
  the layers mount volatile under it.
- A daemon of this change and an image of 0.1.2: asked, the image installs the filter and mounts its layers plain;
  doctor says its runners skip sync calls only.
- A slot whose volume was last mounted volatile and is now mounted plain, after a reload or an older image, mounts:
  the directory goes first whatever is asked. An image older than this change does not remove it, so a slot that ran one after a volatile mount
  needs its volume emptied first.

## Evidence

`crates/core/tests/slot_docker_store.rs`, against Docker and the image built from the tree, once under sysbox and
once under `unsafe-dind`:
`a_slots_layers_skip_their_unmount_sync_and_mount_again_after_any_ending_under_sysbox` and `..._under_unsafe_dind`
read the image's labels, then run jobs on one slot that each read `/proc/self/mountinfo` (`fsync=volatile` on the
layer) and what the last job left: asked twice, then not asked, then asked, then asked with `mount` refusing
`volatile`, then asked; then a job killed with `docker rm -f` while its layers are mounted, and one more after it.
Every job mounts its layers again and finds what the last one wrote; the refusal is said only where it happened.
Without the removal of `work/work/incompat`, the second job falls back to a plain mount and the test fails.

The crash cases are ADR 0141's: `what_runners_wrote_unsynced_is_emptied_after_a_crash_and_only_then` empties
every slot's volume, layers and work directories included, before any runner starts after a boot that did not end
in an orderly shutdown, and keeps them after one that did. An emptied volume has no work directory to refuse.

Measured on the owner's host (btrfs, kernel 7.2, Docker 29.9.3, sysbox), with the image built from the tree and
live CI running 4 to 14 runners beside it, so each count includes the host's background: 1 to 91 commits a minute in
the idle minutes around the runs. The filter was on in every run; only `volatile` differs, refused by a `mount` that
fails on it in the runs without. Three rounds, alternated; commits of the whole file system
(`/sys/fs/btrfs/<uuid>/commit_stats`):

| 10 runner containers started and stopped | round 1 | round 2 | round 3 | mean per runner |
|---|---|---|---|---|
| mounting no layers: the host's Docker alone | 57 | 42 | 49 | 4.9 |
| layers plain | 59 | 62 | 65 | 6.2 |
| layers volatile | 40 | 43 | 46 | 4.3 |
| layers volatile, and the runner's Docker Engine started, idle | 42 | 49 | 56 | 4.9 |

Volatile layers took 19 commits off every 10 runners in each round, about a third of what a runner's start and stop
forces, down to what the host's Docker forces alone. In the first, quieter run before these, the two differed by 3
(48 against 45): btrfs merges a forced commit into one already running, so on an idle host the layers' syncs mostly
ride along with the host Docker's own, and on a busy one each forces its own.

Inside one runner, the same load either way (no difference beyond noise): 10 `docker run`s 30 to 43 commits, a
three-step `docker build` 6 to 9, a buildx `docker-container` builder created, used and removed 11 to 20, postgres
started, written and stopped 3 to 4. That is the runner's Docker Engine, which this change does not reach.

With `volatile` in containerd's snapshotter options (containerd 2.4.2 started by the image with its own config, and
dockerd on it), `docker run alpine` failed at once: `failed to mount .../rootfs/overlayfs/<id>: ... volatile,
userxattr,index=off, err: invalid argument`, the second mount of the container's snapshot after its create.

## Consequences

- Stopping a runner no longer forces a btrfs commit for each of its layers (two in `base`, three where the Android
  SDK is a layer): about 1.9 commits per runner on a busy host.
- The runner's Docker Engine, its containers and its builds still force one commit per overlay unmount: about three
  per `docker run` and one or two per build step. That is the larger share, and it waits on Docker.
- A host crash costs what ADR 0141 says, no more: the guard already empties the slots' volumes.
