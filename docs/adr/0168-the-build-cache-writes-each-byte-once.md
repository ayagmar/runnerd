# 0168. The build cache writes each byte once

Status: **Accepted** (2026-11-24). Amends [0052](0052-shared-build-cache.md) and [0082](0082-github-actions-cache-served-from-the-host.md).

## Context

The owner's build cache holds about 33,600 entries (30 GiB) and answers about 28,000 lookups a week, half of them
hits. Four of its paths wrote or worked more than they needed to:

- A hit set the entry's file time every time, so the order of use outlives a restart (ADR 0052): one inode write
  per hit, thousands an hour in a Bazel or Gradle build, though the index already orders entries while the daemon
  runs.
- A GitHub Actions cache save sent as one block was copied into a new file, its archive, and the block removed:
  every byte written twice.
- An undecided job's upload of an entry the cache already has, as Bazel sends every blob of its outputs again, was
  held until the decision, taking room and a place among the 10,000 a job may hold, to replace the entry with the
  same bytes or be dropped.
- Every trim cloned and sorted the whole index, also when the entries already fit.

## Decision

- **A hit sets the file's time at most hourly.** The index keeps, beside when an entry was last used, the time its
  file says; a hit sets the file's time only once that is an hour old. A restart orders entries by their files'
  times, so it orders them to within an hour, which a least-recently-used trim of a cache trimmed at most hourly
  does not notice. The file is opened under the index's lock, as an upload is renamed over it and a trim removes it
  under that lock, so a time set late lands on the file it was meant for, never on one that replaced it.
- **One block is the archive.** A block list of one block renames the block to the archive instead of copying it,
  and syncs its data as a join does. A rename that fails falls back to the copy.
- **An undecided job's duplicate is dropped at once.** An upload of the same length as the entry the cache has is
  read and dropped, and the entry marked used, for an undecided job as for one that may write. Held, it would at
  best replace the entry with what Bazel's content-addressed key already names, and at worst be dropped; it counts
  as neither stored nor refused.
- **A trim under the limit does nothing**, before it looks at the entries.

Entries are served from one read buffer per request, reused once the server has sent a chunk, and the build cache's
and GitHub Actions cache's reads share it.

## Consequences

A hit writes nothing to the disk unless its entry's file time is an hour old. After a restart, entries used within
the same hour before it may be trimmed in another order than they were used.

An undecided job whose upload has the length of the entry but other bytes no longer replaces it once it may write, as
a job that may write never did.
