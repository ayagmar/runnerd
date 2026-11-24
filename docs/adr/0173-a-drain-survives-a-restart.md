# 0173. A drain survives a restart

Status: **Accepted** (2026-11-24). Amends [0113](0113-a-released-runners-slot-does-not-wait-for-its-registrations-delete.md)
and [0125](0125-a-start-runs-its-io-in-a-task-of-its-own.md): the runners listing that deletes what they left reaches
a removed repository after a restart too. Adds `draining:<repository>` keys to the history's `meta` table, with no
schema step, `Store::drains`, `Store::draining_repositories`, `Store::path`, `Setup::draining` and
`Setup::default_database`. No wire or config change; a config that moves `history.database` is refused while a
repository drains.

## Context

A repository a reload removes drains: the engine goes on listing its runners and deletes this machine's
registrations there, until two listings in a row have none. Some registrations have no container and wait for that
listing alone: a start that failed after GitHub registered its runner, and a released runner, when GitHub's rate
limit refused the background delete.

A restart rebuilt the drains only from the running runner containers it found. A removed repository whose only trace
was such a registration was not drained again: nothing listed it, and the registration stayed on GitHub for good.
The order that loses it is ordinary: remove the repository while GitHub's rate limit holds, then restart the daemon
before the limit lifts.

## Decision

- The history keeps the draining repositories as `draining:<repository>` keys of `meta`, value 0, synced to disk. A
  write names the drains that began and those that ended; nothing else changes, so no write drops a record another
  path still needs.
- A config change records the drains it begins before its file is written. If the history cannot record them, the
  change is refused and the file is left as it was.
- A config change that adds a draining repository back forgets its record only once its file is known to last. When
  the commit warns that a crash may bring back the old file, the record stays: a start under the old file drains the
  repository again, and a start under the new one forgets the record, as it finds the repository listed. A record
  left by a commit that failed is forgotten the same way.
- A config change that would have the next start open another job history (`history.database`, or the default path
  when it names none, against the path the live history was opened at) is refused while any repository drains,
  whether this change removes it or an earlier one did: the other history knows nothing of the drain.
- Recovery records a drain it begins for a running runner. A drain that ends is forgotten without waiting for the
  write; if that write is lost, the next start drains the repository again until two clean listings end it.
- On start, the engine drains every recorded repository the config does not list, beside those of the running
  runners it finds. Each starts its drain over, as a reload does.
- Keys in `meta` rather than a table: the set is small, and a schema step would be the only one in this change.

## Consequences

A registration of a removed repository is deleted once GitHub answers again, whether or not the daemon restarted in
between, or a crash undid the config that added it back. A drain that gave up after an hour of failed listings, or
one GitHub said is gone, is not kept. A config change that removes a repository now fails if the history cannot
sync, as an auto sizing change already does, and one that moves the history waits until no repository drains.
