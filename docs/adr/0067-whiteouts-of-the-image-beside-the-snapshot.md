# 0067. What a job deleted of the image is hidden by whiteouts beside the snapshot

Status: **Accepted** (2026-10-25). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md).

## Context

`image/seed.sh` copies slot 1's view of a layer, its changes over the snapshot, to make the new snapshot. That view
leaves the image out, so a whiteout or opaque directory in slot 1's changes that hid one of the image's files hides
nothing there and is not copied. Every slot then saw the image's file again: a file a job deleted came back, and
a directory a job replaced, such as an SDK package an update reinstalled, showed the image's old files beside the
job's ([#116](https://github.com/ayagmar/runnerd/issues/116)).

Copying whiteouts into the snapshot does not work. The next reseed's view has no layer under the snapshot, so its
directories are read as they are, whiteouts listed, and `du` and `cp` fail on them. Opaque directories cannot be
copied either: under sysbox, `getxattr` of `trusted.overlay.opaque` fails in the seed container.

## Decision

- A reseed mounts slot 1's whole view, its changes over the snapshot over the image, and walks the image's path.
  Each image entry that view hides gets a whiteout, a `0:0` character device, in `/base/<layer>.hidden`; what is
  under it is not looked at. A replaced directory is a whiteout for each entry of the image's that it no longer has.
  Nothing under a symlink or a file of the view is looked at, and the layer holds only the directories the walk
  makes, each with the image's owner and mode.
- A runner mounts each layer as its slot's changes over the snapshot over `/base/<layer>.hidden` over the image.
  The snapshot is copied from a view without that layer, so it never holds a whiteout.
- The whiteouts are worked out again at every reseed, from a view that includes the last ones, so they carry
  forward.
- The image's own `CACHE_SECRETS`, and their directories, get none: the image's copies stay, as `mount_layers`
  keeps them.
- Starting over drops the layer with the snapshot. A layer missing from the image gets none.

## Consequences

The whiteouts are made against the image of the reseed that made them: after a change of image, an entry the
new image adds under a directory a job replaced shows until a job deletes it. An image older than this ignores
`/base/<layer>.hidden`, and its seed script leaves it as it was, so after going back to one, the image's deleted
files show again, as before. A reseed walks the image's layer paths once more; the published images hold a few
hundred entries there.
