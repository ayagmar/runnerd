# 0039. Auto sizing: the daemon applies advice itself, a step at a time, off by default

Status: **Accepted** (2026-10-23). Amends [0032](0032-sizing-advice-from-sampled-peaks.md).

## Context

Sizing advice ([0032](0032-sizing-advice-from-sampled-peaks.md)) says what each profile's `memory` and `slots`
should be, and someone has to apply it: `runnerctl sizing apply`, the TUI or the dashboard. The owner wants runnerd
to follow its own advice as jobs come and go. A daemon writing its own config has to stay as safe as a person
applying advice: it must not write a config the budget check refuses, swing caps on one heavy week, lower a profile
whose jobs were just killed for memory, or take slots away from runners in them. And the file must stay the owner's:
comments kept, edits never overwritten, every change visible and undoable.

## Decision

`[sizing] auto = true` turns it on; it is off by default. When the daemon starts and every 5 minutes after, it reads
the advice for the config it runs with, and `sizing::auto::changes`, pure, picks what to change:

- `memory` changes only when a profile's advice is to lower or raise it, which takes `[sizing] min_jobs` runs. Under
  `measured` a profile can get a slot without a memory change, by the slot rule below, also after a run met its cap.
- At most one change per profile per `[sizing] interval` (default 24 hours), counted from the profile's last change
  however old, pending or written.
- A change moves `memory` by at most `[sizing] max_step` percent of it (default 25), in steps of 256M. A profile far
  from its need gets there over several intervals, each sized by the runs since.
- A profile stays within its `min_memory`, `max_memory` and `max_slots`. The config check refuses a profile already
  outside them, so a change never has to jump a bound; a change whose result would leave them is not made.
- No lowering while any run of the profile in the history's 30 days was killed for memory or met its cap, cancelled
  runs included. The advice already holds this for finished runs; cancelled ones count here because a kill for
  memory can end in a cancel.
- `slots` are never lowered under what runs. Under `caps` a raise that leaves room for fewer slots than run waits
  until fewer run; a lowering adds slots as the advice does only up to `max_slots`, and without it keeps them.
  Under `measured` a profile whose jobs waited gets one more slot per change, up to `max_slots`, when every
  profile's slots at their jobs' p95 peaks (at their caps with too few runs) and the mirror fit `MemoryMax`.
  Without `max_slots` auto sizing never adds a slot.
- Changes in one look are picked in order, each against the budget the ones before it take: two profiles never both
  take the one slot or the one spare budget that fits. The config check alone cannot hold that under `measured`,
  where it checks one runner per profile.

The daemon applies each change as `PUT /v1/config` does, through the same function (`api::control::replace`): the
text is the file with the profile's `memory` and `slots` replaced by `config::resize_profile` (comments and layout
kept), checked against the slice's `MemoryMax` read now, and written by the engine between ticks. Three
preconditions keep it from overwriting anything newer or taking a slot away:

- The file on disk must be the text runnerd runs with. While it differs, as after an edit not yet loaded, auto
  sizing changes nothing and says so once; the commit checks it again just before writing.
- `PUT /v1/config?revision=` refuses (409) a text made from another revision than the one runnerd runs with
  (`config::revision`, a hash of the text, in `GET /v1/config`), and one sent while the file on disk is not that
  revision's text, which the commit checks again. Its reply has the revision of the text it wrote, which the
  dashboard's editor saves against next: never a later read's, which may be of a config written since.
  `runnerctl sizing apply` and `undo`, the TUI and the dashboard's editor send it, so an editor opened before an
  auto change or an edit on disk cannot save over it. Without the parameter a `PUT` replaces whatever runs, as
  before.
- The engine checks, between ticks and before the commit runs, that no runner is in a slot the new config lacks
  (`RunningSlots::Keep`). A person's reload keeps draining such slots (`RunningSlots::Drain`).

Each change is recorded in the job history before its file is written: a row in `auto_sizing` with when, the
profile, the old and new values, why, and `pending`. Once the file is written it becomes `done`, in the same
transaction as its durable `sizing.changed` event; if the write was refused or failed before the rename, `failed`.
Both transactions commit with `synchronous = FULL`, synced to disk before the file is written or the answer
returns, so a power failure cannot keep a synced config file and lose its record. The writer goes back to `normal`
for everything else. A change the history cannot record is not written.

One left pending, by a crash or a record that failed after its file was written, is settled at the next look,
including the one at start and whether auto sizing is on or not: `done` if the profile has the new values, else
`uncertain`. Other values do not prove the file was not written, as a person may have changed them since; an
uncertain change is listed with what it replaced and holds the interval like a done one. Until settled, a pending
change holds the interval. The table is never pruned (a few rows
a day at most), so the interval holds however long it is. `GET /v1/sizing` lists the done changes in its window;
undoing one writes its old values back (`runnerctl sizing undo`, the dashboard's link into the config editor).
Older clients skip the event type and the new optional fields.

Off by default: auto sizing rewrites the owner's file and changes the caps jobs run under. A config that worked
yesterday keeps working the same after an upgrade.

## Consequences

The file stays the source of truth: restarting reads what auto sizing wrote. A person edits it through runnerd (the
dashboard, or `runnerctl` and a restart); an edit on disk alone pauses auto sizing until runnerd runs with it, and is
never overwritten. A manual change does not restart the interval.

The history's schema goes to version 9; as after every migration, an older runnerd refuses the database.

A step limit slows a raise too: a profile whose jobs hit their cap gets at most `max_step` more per interval. The
advice raises it to the largest peak plus the margin, and a run at the cap keeps it from being lowered again, so the
cost is time, not a wrong cap. The owner can raise `max_step` or apply a raise by hand.

A change refused by the check or the engine is recorded as failed and holds no interval: the next look tries again,
and its refusal is logged once until it changes. Two synced commits per change cost a few milliseconds a day.
