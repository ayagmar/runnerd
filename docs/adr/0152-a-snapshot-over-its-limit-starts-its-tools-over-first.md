# 0152. A snapshot over its limit starts its tools over first, and all of it only when that is not enough

Status: **Accepted** (2026-11-20). Amends [0065](0065-a-snapshot-starts-over-from-the-image.md): what a reseed over
`snapshot_max` drops. No config or API change.

## Context

Under 0065 a reseed whose snapshot would pass `snapshot_max` starts every layer over from the image, and every slot
of the profile runs its next jobs cold: Gradle, npm and the tools their setup steps download. On the owner's host the
jvm profile started over 11 times from 2026-11-01 to 11-18 and node 4 times. After the first, which went over the 10G
limit by 4.8G, each went over by 0.07G to 1.65G, while the tools layer held 1.6G to 4.2G at the reseed before and the
cache layer 4G to 8G. One start-over, on 11-17, was followed by 9 jvm jobs failing to download their tools.

Two ways to keep more:

- Prune the cache layer with each cache's own cleanup (Gradle's, `npm cache verify`, `pnpm store prune`). Each needs
  its tool, at the version that wrote the cache, run as root in the seed container over files jobs wrote: a job could
  leave a Gradle distribution or a pnpm binary there for the reseed to run. Gradle's cleanup also removes only what
  went unused for days, which may not be enough.
- Start over only some layers. Each layer (`cache`, `tools`, `android`) is an overlay of its own over its own path, and
  no cache's index names files in another layer, so a layer that starts over from the image leaves the others' caches
  whole. The image is whole on its own, as 0065 needs.

What a kept layer runs from a dropped one is the exception. The image's `PIPX_HOME` is in the cache layer, and so are
Poetry's virtualenvs and pre-commit's: a venv made with the Python of `actions/setup-python` holds a `pyvenv.cfg` and
a `bin/python` link that name an interpreter under `_work/_tool`. Kept without the tools layer, it is broken rather
than cold, and pipx does not make it again on its own. Files that only name such a path as data, as pip's self-check,
virtualenv's interpreter cache or Gradle's file hashes do, are checked against the file system by their cache and
miss.

The data favours layers: dropping the tools layer, at its size at the reseed before, would have brought 14 of the 15
start-overs under the limit and kept Gradle's and npm's caches.

## Decision

- A reseed weighs the snapshot it would make per layer, as 0065 measures it. Over the limit, it starts the layers of
  `START_OVER_FIRST` (`image/layers.sh`: tools, then Android's SDK) over from the image, in order, until the rest fits.
  Each layer that starts over loses its snapshot, its whiteouts and every slot's changes to it.
- The weight of a layer slot 1 changed counts the whiteouts its new snapshot will publish, made before it is weighed
  and then published as weighed. A profile's first reseed, with no snapshot yet, makes them over the image alone.
- Before it drops a layer, the reseed looks in the snapshot each kept layer would get for what runs from the dropped
  layers' paths: a symlink that resolves into one, and an executable or `pyvenv.cfg` whose first 4 KiB name one
  anywhere, so a launcher that execs the interpreter on its third line is found too. The look lists the layer into a
  file and checks that `find` succeeded, then reads each candidate once, at most 4 KiB, never through a symlink, in one
  `python3 -I` of the image's; it runs nothing it finds. Bash's `read -N` was not used: it skips NUL bytes without
  counting them, so it would read a file of zeros to its end.
- A hit inside one of the cache layer's known Python environments drops that environment with the layer:
  `pipx/venvs/*`, `pipx/.cache/*` (`pipx run`), `pypoetry/virtualenvs/*` and `pre-commit/repo*/py_env-*`, each a real
  directory one level below its root with a real `pyvenv.cfg`. pipx, Poetry and pre-commit keep each environment's
  metadata inside it and make it again when it is gone. Any other hit, any hit in another layer, and any failure to
  list or read start the whole snapshot over, and the output says why. No ancestor is pruned for holding a
  `pyvenv.cfg`.
- The layers that stay are made as below the limit: slot 1's view of each one it changed becomes its snapshot, and
  every slot's changes to it go.
- The cache layer is never started over alone. When the first layers are not enough, the whole snapshot starts over
  from the image, as under 0065.
- A reseed that did not finish still starts over whole (0066), and nothing else changes in what a snapshot keeps or
  drops of a job's files (credentials, Gradle's daemon registry, whiteouts).
- The seed script says which layers it started over: `…, over its limit of …: started tools over from the image,
  keeping … bytes`, with how many Python environments went with them, or why the whole snapshot started over. The kept
  bytes are the published snapshot weighed once, after the layers are seeded, so a file linked from two places counts
  once. `api::reseed_outcome` picks the line of a reseed's output that says
  what it did, a start-over's before the last, which the TUI shows; `runnerctl reseed` prints the output whole. The
  limit's label and the upkeep log say only that the snapshot is cut back at its next reseed: the profile's image runs
  the seed script, so what goes is what its output says.
- A reseed ends only once Docker says its seed container exited or is gone. When the wait and the removal both fail,
  as when the Engine's API drops, the container may still be deleting a layer or publishing one, and a runner started
  then would mount it half-written: the reseed asks Docker again every 10 seconds, and the profile starts no runner
  meanwhile. That wait runs only in the reseed's and the measure's background tasks. The tick starts a slot's layers
  over after a host crash itself, so that container gets one attempt per tick: if Docker cannot say it ended, the
  caches stay torn, no runner starts, and the next tick removes that container before it starts another.

## Consequences

A profile whose snapshot passes its limit by less than its tools layer keeps its Gradle and npm caches; its jobs fetch
their tools again, which the image may already hold, and remake the Python environments that ran from them. A cache that
runs from the tools layer anywhere else keeps the whole start-over, which is the rule of 0065: the look can only make a
reseed start over more. A reference past a file's first 4 KiB, or in a file that is neither executable nor a
`pyvenv.cfg`, is not found; ELF interpreters and RPATHs into the tools layer are the known case. As the cache layer grows towards the
limit, the tools layer starts over at more reseeds, until the cache layer alone passes the limit and the whole snapshot
starts over. Pruning by each cache's own cleanup stays undone, for the reasons above.
