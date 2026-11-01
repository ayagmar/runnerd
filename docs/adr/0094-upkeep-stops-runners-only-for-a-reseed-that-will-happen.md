# 0094. A profile's upkeep stops its runners only for a reseed that will replace the snapshot

Status: **Accepted** (2026-11-01). Amends [0050](0050-warm-runners.md): a job in slot 1 no longer makes a reseed
due by itself.

## Context

Once slot 1 ran a job, its profile was due to reseed. Upkeep then waited for the profile's runners to be idle, stopped
its warm runners, started none until the seed container ended, and started them again after. The seed container
(`image/seed.sh`) only then weighed slot 1's changes against `[caches] reseed_at`, and below it did nothing.

On the owner's host after 0.0.26, that was most reseeds: 135 of 140 printed "not seeded: slot 1 changed N bytes".
About a third of all idle removals (162 of 479) stopped warm runners for one of them, each costing the profile's warm
runners, their registrations and containers, and its starts while the seed container ran. Slot 1 of `light` stayed at
the same 201 MB for hours and was reseeded again after each of its jobs; `jvm`'s slot 1 grew from 660 MB to 880 MB in
two hours against a `reseed_at` of 2G.

## Decision

- **Slot 1 is measured first, beside the profile's runners.** A job that ends in slot 1 makes a measure due, not a
  reseed. `cache::measure` runs `seed.sh measure` in a container that mounts slot 1's volume and the snapshot
  read-only, so it holds no slot and stops no runner. It prints the bytes of slot 1's changes to the layers, without
  its Docker Engine's data (`layer_changes` in `image/layers.sh`, which the reseed weighs too), or that the last
  reseed did not finish.
- **Only a measure of `reseed_at` or more makes the reseed due**, and the upkeep that stops the profile's runners
  with it. One below it changes nothing until slot 1 runs another job. A reseed the last did not leave finished
  (ADR 0066) is due whatever slot 1 changed.
- **A measure that fails makes the reseed due**, as every slot-1 job did before: the reseed measures slot 1 itself and
  reports its own failure in `cache.reseeded`. A measure fails on a Docker or sysbox error, or when the profile's
  image predates `seed.sh measure`; not on a file a job deletes while it counts.
- **Unchanged:** a requested reseed (`runnerctl reseed`) runs once the profile's slots are free, whatever slot 1
  changed, and one of a snapshot the caches' check found over `snapshot_max` starts it over. The reseed still checks
  `reseed_at` itself, so a measure that is stale by the time it runs replaces nothing. The shared caches' check stops
  warm runners an hour after the last, as before.
- **When it runs.** A measure starts once a slot-1 job ended since the last, at most every `MEASURE_EVERY` (10
  minutes) per profile, when slot 1 runs no job and Docker has the profile's image, and not in the tick that started
  slot 1's runner, whose container may still be dropping layers as it mounts them. A job slot 1 takes meanwhile can
  skew the number, which the reseed checks again; `du`'s complaints about files deleted under it are left out rather
  than failing the measure. It does not start for a profile already due to reseed or reseeding, and a reseed waits for
  a running measure to end, since the reseed drops the layers it reads. A job that ends in slot 1 within 10 minutes of
  a measure waits for the next. The reseed happens up to 10 minutes later than it could, and changes nothing that
  matters before then.
- **What it costs.** A container start in the profile's isolation, capped at one CPU and 256M and not reserved in the
  budget, plus `du` over slot 1's layer changes: the work each no-op seed container did, minus stopping and
  restarting the warm runners. On the owner's host those seed containers ended within the tick that started them.
- **A restart** removes a measure container an earlier run left, which recovery finds beside the seed containers
  (they share the `runnerd.seed` label), and slot 1 is measured again after its next job. A reload that removes the
  profile drops its measure's result; a reload that changes `reseed_at` applies from the next measure.

## Consequences

- A profile whose slot 1 changes little keeps its warm runners through its jobs. In the window measured, `light`
  (201 MB) and `jvm` (under 900 MB) would not have stopped theirs for upkeep; only reseeds that replaced the
  snapshot, 5 of 140, would have.
- The snapshot catches up with slot 1 up to `MEASURE_EVERY` later than before, when slot 1 has grown past
  `reseed_at`.
- Slot 1's growth shows in the log every measure: "light: slot 1 changed 191.69M, under reseed_at (2G): no reseed".
