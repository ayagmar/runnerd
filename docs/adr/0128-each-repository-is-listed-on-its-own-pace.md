# 0128. Each repository is listed on its own pace

Status: **Accepted** (2026-11-10). Amends [0111](0111-a-tick-is-due-an-interval-after-the-last-one-started.md): a
scheduled tick is due when the first repository is, and lists only the repositories due. Amends
[0127](0127-reads-are-paced-by-the-rate-limit.md): the governor paces each repository's listings, not the scheduled
ticks. No wire, schema or config change.

## Context

Every tick listed every repository's queued jobs and runners, every `poll_interval` (15 s by default). A job of a
repository that is busy waited 7.5 s on average before runnerd saw it, the same as a job of one that had seen nothing
for a day. Listing every repository more often costs requests for the idle ones too. The listing order and the newly
busy runner's claim ([0119](0119-a-slots-runner-is-one-engagement.md)) were sound only because each tick listed every
repository's runners and then its queued jobs.

## Decision

- **Each listed repository has its own pace**, a `ListingPace` in its `RepositoryState`
  ([0121](0121-a-repository-keeps-its-pools-in-one-map.md)): when its last listing began, on Tokio's clock, and in
  how many probes its runners are listed again. Its due time is derived when read, so a reload's `poll_interval` and a
  governor step apply at once.
- **Cadence:** `poll_interval / 3` (5 s) while the repository is active, `poll_interval` while it is idle. Active means
  not paused and either active by [0107](0107-a-warm-runner-waits-only-where-jobs-just-came.md)'s rule (a job of it
  listed queued, taken or finished in the last ten minutes) or a runner of it holds a job as far as the engine knows
  (listed busy or named by its capture). The widening is for the listings only: warm runners still follow ADR 0107's
  rule alone, so a warm runner does not keep its own repository active. A paused repository is idle. A failed listing
  keeps the cadence the repository had.
- **The study's other widening, "its last listing showed a job", is left out:** ADR 0107's rule already counts a job
  a pool matches from the listing that shows it, for ten minutes. Counting every listed job would keep a repository
  at 5 s for the 24 hours GitHub keeps a job queued that no runner can take (a typo'd label, a hosted runner's).
- **Ticks:** a scheduled tick is due when the first repository is, and at least `TICK_REST` (2 s), or the shortest
  interval if that is less, after the last tick ended. It lists the repositories due. With no listed repository it is due an idle
  interval after the last tick began. The first tick, and a tick a command, a wake-up or a stop brings, lists every
  repository and its runners, as before.
- **Runners listings:** a repository's runners are listed at every third probe of it, at the next probe after one of
  its runners started or was released, or after its runners listing failed, and at the first probe after a restart
  or a reload that adds it, since adopted runners count as busy until a listing answers for them. A draining
  repository's runners are listed at every scheduled tick, as its drain ends on two clean runners listings. A stop's
  ticks list every repository's runners and no queued jobs.
- **A newly busy runner's claim ends per repository.** The mark of the listing begun last (its number and when it
  began) is set when a runners listing first shows the runner busy, and is never moved by a later one. The claim ends
  at the answer of its own repository's queued listing that began after the mark and at least `poll_interval` after
  it, whatever other repositories' listings answer. A runner that went with no job named claims a listed job by the
  same rule, from the listing begun last before it went. The floor is the margin for GitHub's queue lag that a 15 s
  cadence gave: the probe saw a runner's job leave the queued view within 10 s in 97% of 87 cases, and within 50 s in
  all of them.
- **No `updated_at` skip.** The study proposed reading a run's jobs page only when the run's `updated_at` moved. The
  probe (30 minutes, every 10 s, a busy repository) found it did not move in 48 of 62 jobs-page changes caused by a
  job's status: the skip would miss jobs. Jobs pages stay conditional requests; 67% of them, and 88% of the runs
  listings, came back 304, which do not count against the primary limit.
- **The governor ([0127](0127-reads-are-paced-by-the-rate-limit.md)) paces each listing:** an idle repository's
  interval is `listings_every(poll_interval)` (`poll_interval`, then 2× and 4× at its last two steps), an active one's
  `active_listings_every(poll_interval)` (a third of it until `ActiveListings`, then as an idle one). Undoing
  `ActiveListings` counts as tripling what it saved.

## What survives a restart and a reload

Nothing of a pace survives a restart: every repository is due at once and its runners are owed. A reload keeps the
pace of a repository the config still lists; one it adds starts as after a restart, and one it removes drops its pace
with its state and drains. Marks and claims are the engine's and do not survive a restart, as before.

## Consequences

- A busy repository's jobs are seen sooner. On the fake clock, with one repository running four jobs and two idle,
  from a job's queueing to the listing that showed it: p50 7.6 s and p90 13.6 s before, 4.1 s and 4.8 s after; to its
  container start, p50 8.9 s and p90 14.9 s before, 5.5 s and 6.0 s after.
- An idle repository's first job is seen as before: p50 8.2 s, p90 14.2 s. The gain is for repositories already active.
- More requests: in that model, 1,145 queued listings an hour instead of 723, 460 runners listings instead of 722.
  Computed with the probe's 304 shares, about 2,760 an hour count against the primary limit instead of about 2,280
  (55% of 5,000 instead of 46%). The governor slows the active listings first when the limit runs low.
- A tick a command or a wake-up brings sooner than `poll_interval` no longer ends a claim: the floor holds it until a
  listing at least `poll_interval` after the mark. At one cadence the trace is unchanged.
- In a pool whose capture is slow, a newly busy runner holds one job back for up to `poll_interval` after the listing
  that showed it busy, as before.
