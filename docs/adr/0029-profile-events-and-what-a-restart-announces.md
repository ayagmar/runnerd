# 0029. Profile events, and what a restart announces

Status: **Accepted**. Amends [0027](0027-pruning-runs-beside-the-writer.md).

## Context

A client keeps its status current by applying events. `slot.changed` names a profile and a slot, so it cannot add a
profile a reload added, drop one a reload removed, or drop slots past a lowered `slots`. A profile starting or
stopping a reseed had no event at all, so the dashboard read the whole status on every event to catch it.
Repositories already had `repository.changed` and `repository.removed`, and a restarted daemon compared its config
with the last ones stored, so a client resuming with `Last-Event-ID` learned of a repository removed while runnerd
was down.

## Decision

- `profile.changed` carries a profile as it is now, slots included. It is sent when a profile appears, or its
  image, CPUs, memory, reseeding or slot numbers change. A runner coming or going in a slot stays `slot.changed`
  only. `profile.removed` is sent when a removed profile leaves the status, once its last runner and reseed are gone.
- Migration 5 adds `announced_profiles`, each profile's latest `profile.*` event, kept in step by the writer and kept
  by pruning, as `announced` is for repositories. At startup the daemon reads both (`Store::announced`), and the
  first publish announces the profiles that differ from those and the ones the config no longer has.
- The slots in a stored `profile.changed` are older than the `slot.changed` events a client has since applied, so
  the first publish after a start announces every slot rather than comparing slots with them.

## Consequences

A client that applies `profile.*` and `slot.changed` to the profiles of a status keeps them equal to `/v1/status`,
across reloads and restarts. Each start sends one `slot.changed` per slot, as it did before. Clients that do not
know the new events skip them, since they skip events that do not parse.

The first start after the upgrade to migration 5 has no stored profiles to compare with: it sends `profile.changed`
for every configured profile, but no `profile.removed` for a profile removed in the same restart. A client resuming
then keeps that profile until it reads `/v1/status` again. This happens once per machine, as it did for
repositories when `announced` came.
