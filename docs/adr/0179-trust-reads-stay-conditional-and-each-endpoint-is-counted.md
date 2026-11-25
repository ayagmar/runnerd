# 0179. Trust reads stay conditional, and each endpoint is counted

Status: **Accepted** (2026-11-25). Amends [0052](0052-shared-build-cache.md) and
[0053](0053-build-cache-written-by-own-pull-requests.md): the answers GitHub gives the build cache's trust reads are
kept for conditional requests, and a repository's default branch is read once per batch of decisions. Amends
[0127](0127-reads-are-paced-by-the-rate-limit.md): the hourly line counts each endpoint of `other` on its own. No wire,
schema or config change.

## Context

Before a job may write to the build cache, runnerd asks GitHub for four things: the job's run
(`GET /repos/{repo}/actions/runs/{run}`), its pull request (`/pulls/{number}`), the branch's head
(`/branches/{branch}`) and whether that head has the run's commit (`/compare/{head}...{sha}?per_page=1`). The client
dropped what each answered right after the read, so a request for the same run, pull request, branch or comparison
could never be a 304. Every job of a 25-job run paid the rate limit four times for answers that did not change. One
production machine counted 919 fresh answers of 978 under `other` in 41 minutes, about 1,345 an hour, and the line
could not say which endpoint they were.

The workflow lookup (ADR 0140) dropped its read of the same run path for the same reason, and the jobs that one
decision batch decided each asked for their repository's default branch. A default branch used again counted as read
again, so under continuous jobs it was never asked again and a renamed default branch never took over.

## Decision

- **A trust read is asked every time, conditionally.** The four trust reads, and the workflow lookup's read of a run,
  keep GitHub's answer in `Github`'s conditional cache, the same bounded cache the polls use (32 MiB, least recently
  used first, no answer used within 10 minutes dropped for room). Every decision still sends its request, with
  `If-None-Match` when an answer is kept: no decision, allowed or refused, is cached, and a run's ID alone is never
  trusted, since a re-run keeps it and changes who triggered it.
- **Only a 304 serves what was kept.** A 304 is GitHub saying the kept answer is current, and costs no rate limit.
  Any other answer replaces it: a success with an `ETag` is kept in its place, one without an `ETag` drops it, and an
  error status (403, 404, 429, 5xx) drops it and is an error. A request that fails before GitHub answers, or that
  the rate limit holds back, is an error and serves nothing. A failed read leaves the job undecided, as before.
- **Large or one-off answers are still dropped.** A commit's tree and a blob (ADR 0147), a completed job, and the
  pages of an attempt's jobs read before a re-run (ADR 0134) are dropped after their read.
- **A default branch is as old as its read.** The jobs of one decision batch share one read of each repository's
  default branch, made before their runs are read. A branch already known is used with the age of the read that
  learnt it, so it is read again an hour after that read however many jobs used it.
- **Each endpoint has a class.** `github::Class` names every endpoint the client asks: the six of the engine's polls
  and runners, then run triggers, other run reads, pull requests, branches, compares, repositories, workflows,
  archive refs, trees and blobs, attempt jobs, cancels, re-runs, and `unclassified` for anything else. A class is
  finite: it never holds a repository, a number or a path. A trust read of a run is `Trigger`, named by its caller,
  because its path is any run read's. The hourly line keeps its six classes and `other`, which sums the rest, and
  follows `other` with each of them that was asked: `other 40 (30 fresh, 10 unchanged) [run triggers 25 (5 fresh,
  20 unchanged), repositories 15 (…)]`.

## Consequences

- The jobs of one run after the first get 304s for an unchanged run, pull request, branch and comparison, which do
  not count against the primary limit. The requests are still sent: the saving is in what they cost, not in how many
  go. How much of `other` it saves depends on how many of its requests are repeated trust reads with an `ETag`, which
  the new classes now show.
- A kept run, pull request or branch answer is a few to a few tens of KB, within the cache's limit. A comparison of
  a commit the branch has lists no files, so it is small; one the branch lacks refuses the job either way.
- What is kept is in memory only. A restart starts with nothing kept and every first read fresh; a reload does not
  touch it. The default branches the engine knows are in memory too, and a restart reads them again.
- A log reader that matched `other N (…);` now finds the bracket between `other`'s count and the semicolon.
