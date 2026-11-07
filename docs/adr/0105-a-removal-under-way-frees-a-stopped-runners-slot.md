# 0105. A removal under way frees a stopped runner's slot

Status: **Accepted** (2026-11-07). Amends [0076](0076-a-slot-is-free-once-docker-removed-its-container.md). No wire, schema or
config change.

## Context

0076 frees a slot only once Docker confirms its container's removal, so a second runner never shares its volumes. After a
`docker rm -f` of a busy runner in the 0.0.29 sandbox run, Docker answered our removal with a 409, `removal of container … is
already in progress`, and our read of its log with a 409, `can not get logs from container which is dead or marked for
removal`. The engine took both for faults: it warned, kept the slot, and logged the runner's end again at the next tick, with
a third warning for the 404 by then.

## Decision

- **`docker::is_being_removed` reads those two 409s, by their words.** Other 409s, such as a running container's refusal or a
  volume in use, are not it, and Docker gives them no code of their own. `docker::is_log_gone` adds the 404, for a log read.
- **A runner that stopped and is being removed frees its slot.** The release reads no log and frees the slot in the tick that
  saw it stop, with no warning. Nothing runs in the container, so nothing uses the slot's volumes, which is what 0076 protects.
- **The container is followed until Docker says it is gone.** The 409 says a removal runs, not that it will succeed: Docker's
  `cleanupContainer` can fail and leave it dead, and our own removal that timed out gets the same answer. So the container goes
  into `leftover`, which asks again each tick. Only a 404 or a success counts as removed. Another 409 for a removal under way
  stays silent, and a real failure warns.
- **A runner being removed that is not known to have stopped keeps its slot.** Docker marks a container for removal before it
  kills it, so `kill` and `remove_idle` stay silent and leave the slot to `collect_finished`, which frees it once Docker says it
  stopped.
- **Leftovers of an earlier run follow the same rule,** in `collect_finished` and `remove_left`.

## Consequences

- These words are Docker's and may change. A new wording brings back the warnings and the slot kept a tick longer, which is
  what happened before, and no worse.
- A removal someone else began logs `removed …` a tick or so after the runner's end, once Docker confirms it.
- A job whose container someone removes has no log to read, so it reads as having run, as it did. Its record is lost with
  the container.
