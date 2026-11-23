# 0156. Each profile's slots get a Docker share by what their stores hold

Status: **Accepted** (2026-11-23). Amends [0054](0054-slot-docker-stores-share-the-disk.md),
[0089](0089-a-slots-cache-mounts-go-last.md). Adds `cache::shares`, the image's `runnerd: docker store` line,
`api::InsightKind::StoreOverflow`, `api::DOCKER_SLOT_MAX` and schema 29 (`runners.docker_held`,
`runners.docker_share`).

## Context

ADR 0054 gave every slot the same share of a quarter of Docker's disk not otherwise used. On the owner's host on
2026-11-23 that was 3 GiB for each of 33 slots, and the history showed what an equal split costs:

- a jvm image job's store held 4 GB after the job in 45 of its last 50 runs. Its cleanup removed every build layer
  and, still over, its images, so the next run extracted its base image again: 2.00 GB written against 1.42 GB with
  the base kept.
- a node image job lost its 1.1 GB xcaddy layer to the routine prune to half the share in about half its runs
  (2.71 GB written against 1.31 GB). That prune removes the least recently used layers whoever used them, including
  the ones the job that just ended built.
- the light (8 slots) and jvm-small (6 slots) profiles had the same 3 GiB a slot and held far less.

Each removal makes the next job download and write the same gigabytes again, so the removal policy itself was a
large part of the SSD writes it exists to bound.

## Decision

- **The image reports what the store held.** After a job, `image/job-cleanup.sh` removes the job's containers,
  volumes, build contexts and git checkouts, then measures the store (`du -sxb`) before the build cache gives way,
  and writes `held=<bytes> share=<GiB>` to the root-only `/run/runnerd-docker-store`. The cleanup that runs when a
  container starts measures nothing: no job ran yet. Once the runner exited and every process of it ended, the
  entrypoint prints `runnerd: docker store held=<bytes> share=<GiB>`, or `runnerd: docker store unmeasured`, to the
  container's log.
- **runnerd keeps it with the runner.** The engine already reads an ended runner's container log as it releases the
  slot; `cache::shares::store_report` takes the last such line, and the store keeps it in `runners.docker_held` and
  `runners.docker_share`. An image older than this prints no line and records nothing.
- **A profile's need is the 90th percentile of what its stores held** after its last 100 reporting jobs of the last
  week (`cache::shares::needs`), with at least 5. The engine reads it from the history at its first tick and then
  once an hour (`scheduler::engine::shares`), so a restart has it before the first runner starts, and a share moves
  at most hourly. A reload keeps it; the new slot counts apply from the next runner.
- **The split** (`cache::shares::slot_shares`), of the same total ADR 0054 computes:
  - a profile with no need yet gets the equal share, so a host with no reports splits exactly as before;
  - the rest goes to the measured profiles' slots in proportion to their need plus a quarter, each slot between a
    floor and `api::DOCKER_SLOT_MAX` (20 GiB, the image's default). The floor is a quarter of the equal share, at
    least 1 GiB and at most the equal share, so the floors always fit and a profile that held little still has room
    for an unusual job;
  - shares are whole GiB, rounded down, and the GiB the rounding freed go back one per slot to the profiles that
    lost most, while they fit. The shares never add up to more than the total, and what the cap leaves stays free.
  On the owner's 33 slots and 99 GiB, with needs of 4, 3, 0.33 and 0.5 GiB, jvm gets 5 GiB, node 4, and light and
  jvm-small 1 each, 99 GiB in all.
- **The disk is bounded as before.** The total is still a quarter of free space plus what the slot volumes hold,
  with free space the least of the file systems down to the disk (ADR 0054, #312). Only its division changes.
- **Layers the last job used stay.** `recent_use` in `image/layers.sh` counts the seconds since the slot's last job
  began (its runner's start, kept on the slot's volume in `/state/.runnerd-last-job`) plus an hour for the current
  job. The routine prune to half the share passes `--filter until=<that>s`, so it removes only layers neither job
  used. The same filter is on BuildKit's GC rule for layers, written at the container's start. Over the share, the
  cleanup removes the layers no recent job used first, and every layer only if the store is still over. Moby 29.9.2
  turns `until` into BuildKit's `KeepDuration`, which skips records used within it and prunes the rest least
  recently used first until the build cache fits `--max-used-space`
  (`daemon/internal/builder-next/builder.go:toBuildkitPruneInfo`, buildkit `cache/manager.go:pruneOnce`).
- **An insight when a profile keeps overflowing.** When at least 10 of a profile's last 20 reporting jobs in the
  window held more than their share, and half or more did, insights add a notice (`store-overflow:<profile>`),
  worded in core (`Insight::store_overflow_sentence`): how many jobs, the p90 held against the share, what that cost,
  and the cause, either that the share is at the 20 GiB cap or that only more free disk raises every share.
- Auto sizing's disk check (ADR 0090) still reasons on the equal share. The mean share per slot is the same, and a
  new slot's profile share follows its need within the hour.

## What a job can and cannot do to it

A step writing the `runnerd: docker store` line to its own output does nothing: step output goes to the job's log,
not the container's. A step running as `runner` that writes into the listener's output reaches the container log,
but the entrypoint's line comes after every process of the runner ended, so it is never the last line. A step that
uses the container's Docker Engine as root can change the image's scripts, as ADR 0155 says of the probe. The most
that does is move its own profile's share toward 20 GiB at the cost of the other profiles' shares, within the same
total: disk use stays bounded, and the history shows the figures.

## Consequences

- A profile's share follows what its stores hold, so the stores are emptied less often and the next job writes less.
  The first runners after an upgrade split equally until each profile has 5 reports.
- What a store holds includes what earlier jobs left, so a larger share lets a profile keep more and report more. The
  total is fixed, so this moves shares toward what each profile's jobs add per run, not toward the cap for all; the
  cap and the floor bound it.
- A share that drops applies at the slot's next container start, whose cleanup prunes to it, as before.
- Schema 29: a 0.1.5 daemon refuses the new history, as after every migration. `StoreOverflow` is a new insight kind;
  0.1.5 clients read it as `Unknown` and show its sentence.
- The image changes ship with this release's runner image. A 0.1.5 image prints no report and keeps the old pruning;
  its slots keep the equal share.
