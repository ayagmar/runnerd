# 0054. The slots' Docker stores share a quarter of the disk

Status: **Accepted** (2026-10-24). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md) and
[0041](0041-snapshot-and-mirror-limits.md). Amended by [0089](0089-a-slots-cache-mounts-go-last.md): BuildKit's GC
follows the slot's share instead of keeping 512 MB, and the build cache gives up cache mounts last, after images.

## Context

Each slot's Docker Engine keeps its images, layers and build cache on the slot's volume, and `image/job-cleanup.sh`
empties it after a job once it holds over `DOCKER_BUDGET_GB`, which was a fixed 20. Until the fix that let a slot's
Docker store survive its container, sysbox's own directory held the store, so the budget never mattered. Now it
does: the owner's 13 slots could hold up to 260 GB on a disk with 361 GB free, beside the snapshots, the mirror,
records and the host's own use. More slots, from config or auto sizing, raise that further. A fixed budget fits no
host: too large for a small disk, and a knob to hand-tune is what auto sizing exists to avoid.

## Decision

- The slots' Docker stores together get a quarter of Docker's disk not otherwise used: what is free plus what the
  slot volumes already hold. Adding back what the volumes hold means the budget does not shrink as the stores fill
  the disk they were given. Each slot gets an equal share of the profiles' configured slots, in whole GiB, at
  least 1 and at most 20, the old fixed budget.
- The engine measures Docker's free disk and the volumes once at start (`Engine::recover`), in the background
  since the volumes holding Docker stores take minutes to measure, and again with every check of the caches, which
  already measures every volume while no runner runs (`cache::trim_shared`). A runner started before the first
  measure lands keeps its image's default. It keeps
  the total and divides it among the slots when it starts a runner, so a change in slots, from config or auto
  sizing, applies from the next runner on.
- The check of the caches removes a configured profile's slot volumes past its slot count, which a lowered count,
  by hand or by auto sizing, leaves: each holds a whole Docker store no job prunes again, out of sight of
  `runnerctl caches`. A slot opened again starts cold.
- runnerd passes the share as `RUNNERD_DOCKER_BUDGET_GB` in the runner container's environment. The entrypoint, as
  root, writes it to `/run/runnerd-docker-budget-gb` (`keep_docker_budget` in `image/layers.sh`). `job-cleanup.sh`
  reads that file, because sudo starts it from the job-completed hook without the container's environment. A job
  reaches the root-owned file only through the container's Docker Engine, with which it can fill the slot's volume
  anyway. Without a value, as when Docker's disk could not be measured or from an older daemon, the image keeps 20.
- After a job, `job-cleanup.sh` first trims the build cache, least recently used first, to about half the budget
  (`docker builder prune --max-used-space`), and only then empties a store still over the budget. BuildKit's own
  GC floor (`defaultReservedSpace` in `image/daemon.json`) is 512 MB, under the smallest share: the 10 GB
  `defaultKeepStorage` it replaces became a 10 GiB floor GC never prunes below, above most slots' share, so a large
  build cache would have emptied the whole store, images too, after every job.
- A store the slot's Docker Engine does not start on, as one a crash or a full disk damaged, is emptied and the
  engine started once more (`start_docker` in `image/layers.sh`), with dockerd's log in the container's output. The
  store is only a cache; kept, it would fail every job of the slot, and the lowest free slot is the one a quiet
  profile always picks.
- No config key. Hosts with other needs can resize the disk Docker is on; if one needs more, a key under
  `[caches]` can be added then.

## Consequences

- On the owner's host, about 361 GB free plus the current slot volumes gives 13 slots about 6 to 7 GiB each, enough for
  halyard/storefront's images and Testcontainers' images per slot.
- A lowered share applies when the slot's next container starts. Its entrypoint runs the cleanup, which empties a
  store over its budget before any job.
- The build cache gives way first. Emptying the whole store (`docker system prune -af`) happens only when images
  and what the build cache cannot give up outgrow the share; a slot whose jobs' images alone need more than its
  share starts cold after every job, which shows as image pulls in the job logs. The other caches are not counted
  in the quarter; their own limits bound them.
- Images released before this ignore the variable and keep 20 GB per slot.
