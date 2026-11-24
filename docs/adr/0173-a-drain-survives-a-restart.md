# 0173. A drain survives a restart

Status: **Accepted** (2026-11-24). Amends [0113](0113-a-released-runners-slot-does-not-wait-for-its-registrations-delete.md)
and [0125](0125-a-start-runs-its-io-in-a-task-of-its-own.md): the runners listing that deletes what they left reaches
a removed repository after a restart too. Adds `draining:<repository>` keys to the history's `meta` table, with no
schema step, `Store::draining`, `Store::draining_repositories` and `Setup::draining`. No wire or config change.

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

- The history keeps the set of draining repositories as `draining:<repository>` keys of `meta`, value 0. The engine
  rewrites the whole set, synced to disk, whenever it changes: a config change that removes or adds back a
  repository, recovery beginning a drain for a running runner, and a drain that ends.
- A config change records the drains it begins before its file is written. If the history cannot record them, the
  change is refused and the file is left as it was. If the file cannot be written, the set is rewritten as it was.
- On start, the engine drains every recorded repository the config does not list, beside those of the running
  runners it finds. Each starts its drain over, as a reload does.
- A drain that ended is rewritten out of the set without waiting for the write. If that write is lost, the next start
  drains the repository again until two clean listings end it, which costs two listings.
- Keys in `meta` rather than a table: the set is small, and a schema step would be the only one in this change.

## Consequences

A registration of a removed repository is deleted once GitHub answers again, whether or not the daemon restarted in
between. A drain that gave up after an hour of failed listings, or one GitHub said is gone, is not kept. A config
change that removes a repository now fails if the history cannot sync, as an auto sizing change already does.
