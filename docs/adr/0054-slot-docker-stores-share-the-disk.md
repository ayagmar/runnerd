# 0054. The slots' Docker stores share a quarter of the disk

Status: **Accepted** (2026-10-24). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md) and
[0041](0041-snapshot-and-mirror-limits.md).

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
- The caches' check, which already measures every volume while no runner runs, also measures Docker's free disk
  (`cache::trim_shared`). The engine keeps the total and divides it among the slots when it starts a runner, so a
  change in slots, from config or auto sizing, applies from the next runner on.
- runnerd passes the share as `RUNNERD_DOCKER_BUDGET_GB` in the runner container's environment. The entrypoint, as
  root, writes it to `/run/runnerd-docker-budget-gb` (`keep_docker_budget` in `image/layers.sh`). `job-cleanup.sh`
  reads that file, because sudo starts it from the job-completed hook without the container's environment, and the
  job cannot change a root-owned file. Without a value, as before the first check or from an older daemon, the image
  keeps 20.
- No config key. Hosts with other needs can resize the disk Docker is on; if one needs more, a key under
  `[caches]` can be added then.

## Consequences

- On the owner's host, about 361 GB free plus the current slot volumes gives 13 slots about 7 GiB each, enough for
  glowly's images and Testcontainers' images per slot.
- A lowered share applies when the slot's next container starts. Its entrypoint runs the cleanup, which empties a
  store over its budget before any job.
- Emptying is all or nothing (`docker system prune -af`), as before. A slot whose jobs need more than its share
  starts cold every job, which shows as image pulls in the job logs. The other caches are not counted in the
  quarter; their own limits bound them.
- Images released before this ignore the variable and keep 20 GB per slot.
