# 0039. Auto sizing: the daemon applies advice itself, a step at a time, off by default

Status: **Accepted** (2026-10-23). Amends [0032](0032-sizing-advice-from-sampled-peaks.md).

## Context

Sizing advice ([0032](0032-sizing-advice-from-sampled-peaks.md)) says what each profile's `memory` and `slots`
should be, and someone has to apply it: `runnerctl sizing apply`, the TUI or the dashboard. The owner wants runnerd
to follow its own advice as jobs come and go. A daemon writing its own config has to stay as safe as a person
applying advice: it must not write a config the budget check refuses, swing caps on one heavy week, lower a profile
whose jobs were just killed for memory, or take slots away from runners in them. And the file must stay the owner's:
comments kept, every change visible and undoable.

## Decision

`[sizing] auto = true` turns it on; it is off by default. Every 5 minutes the daemon reads the advice for the config
it runs with, and `sizing::auto::changes`, pure, picks what to change:

- Only a profile whose advice is to lower or raise `memory`, which takes `[sizing] min_jobs` runs.
- At most one change per profile per `[sizing] interval` (default 24 hours), counted from the last change recorded.
- A change moves `memory` by at most `[sizing] max_step` percent of it (default 25), in steps of 256M, and keeps it
  within the profile's `min_memory` and `max_memory`. A profile far from its need gets there over several intervals,
  each sized by the runs since.
- No lowering while any run of the profile in the history's 30 days was killed for memory or met its cap, cancelled
  runs included. The advice already holds this for finished runs; cancelled ones count here because a kill for
  memory can end in a cancel.
- `slots` are never lowered under what runs. Under `caps` a raise that leaves room for fewer slots than run waits
  until fewer run; a lowering adds slots as the advice does only up to `max_slots`, and without it keeps them.
  Under `measured` a profile whose jobs waited gets one more slot per change, up to `max_slots`, when every
  profile's slots at their jobs' p95 peaks (at their caps with too few runs) and the mirror fit `MemoryMax`.
  Without `max_slots` auto sizing never adds a slot.

The daemon applies each change as `PUT /v1/config` does, through the same function (`api::control::replace`): the
text is the file with the profile's `memory` and `slots` replaced by `config::resize_profile` (comments and layout
kept), checked against the slice's `MemoryMax` read now, and written by the engine between ticks. It holds the
config lock throughout, so it never overwrites a config replaced meanwhile. A refused change is logged once until
the refusal changes; nothing is written.

Each change is logged and stored as a durable `sizing.changed` event with when, the profile, the old and new
`memory` and `slots`, and why. That event is the record: `GET /v1/sizing` lists the changes in its window, the
interval counts from them, and undoing a change writes its old values back (`runnerctl sizing undo`, the dashboard's
link into the config editor). Older clients skip the event type and the new optional fields.

Off by default: auto sizing rewrites the owner's file and changes the caps jobs run under. A config that worked
yesterday keeps working the same after an upgrade.

## Consequences

The file stays the source of truth: restarting reads what auto sizing wrote, and a person can edit it at any time.
A manual change does not restart the interval; the next auto change still waits for the interval after the last
auto change.

Events are the history: no new table and no schema version, so an older daemon still opens the database. They are
pruned after `[history] events` (30 days by default); an interval longer than that counts only from the changes
still kept.

A step limit slows a raise too: a profile whose jobs hit their cap gets at most `max_step` more per interval. The
advice raises it to the largest peak plus the margin, and a run at the cap keeps it from being lowered again, so the
cost is time, not a wrong cap. The owner can raise `max_step` or apply a raise by hand.

Two profiles are changed one after the other, each checked against the config the first one left, so two raises
that only fit one at a time cannot both be written.
