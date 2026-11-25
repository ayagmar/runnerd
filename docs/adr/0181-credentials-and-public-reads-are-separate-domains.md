# 0181. Credentials and public reads are separate domains

Status: **Accepted** (2026-11-25). Amends [0008](0008-auth-with-the-gh-token.md),
[0134](0134-a-job-action-is-previewed-then-sent-to-github-once.md) and
[0147](0147-runners-read-verified-action-archives-from-runnerds-cache.md). Prepares GitHub App installation
authentication; nothing a user sets or sees changes yet. No wire, schema or config change.

## Context

A GitHub App installation token is scoped to the repositories the App is installed on and expires after an hour. Two
things in `github` assumed `gh`'s token: one that sees every public repository and lasts until GitHub refuses it.

- The action archives read a pinned action's repository (`GET /repos/{owner}/{repo}`) with the same token and the
  same call as a served repository's. A 404 there removes a kept archive. GitHub does not promise how an installation
  token answers for a public repository outside the installation, so a scope refusal could be taken for a
  disappearance and empty the cache.
- `github::token` read the token on a thread and kept it until a 401. An expiring token needs an asynchronous,
  bounded read, a refresh before it expires, and an expiry that holds when the host sleeps or its clock moves.

## Decision

- **A served repository and a pinned action are read through two calls.** `archives::Source` has
  `served_repository`, read with the token on the authenticated budget, and `action_repository`, for another's
  repository. `Meter::ask` takes the `Budget` the read spends: the 1000-request reserve applies to `Served`, and to
  `Public` only while public reads use the token.
- **`PublicReads` says how a pinned action is read.** `Authenticated`, what `Github::new` and `gh` use, reads it with
  the token, as before. `Anonymous` reads it with a client of its own (`github::public`) that has no credential,
  follows no redirect, keeps its own conditional answers, honours GitHub's anonymous limit and `retry-after` in a
  backoff of its own, and sends at most 30 requests an hour, every send counted, a 304 or a failure too. Its answers
  never reach the token's rate-limit reading, its counts, its backoff, the scheduler's pacing or its kept answers.
- **Only GitHub's 404 or 410 is a disappearance.** An anonymous rate limit is `RateLimited`, the cap spent is `Held`,
  and a redirect, a refusal, an answer that does not parse and no answer are errors: each leaves the lookup
  unanswered, so the archives already kept stay and retention holds for the repository whose pins named it, reported
  as `held`. A 429 that gives no time to wait holds the anonymous reads for a minute, GitHub's advice for a secondary
  limit; so does a 403 that names no limit, which may be a secondary limit or a refusal and is reported as the 403 it
  is. A hold never shortens a later one already set.
- **Read anonymously, an unanswered lookup withdraws the runners' view.** Retention and the mount are separate: the
  archives stay in the store, but a refresh publishes the view only once the check found the store good and every
  kept archive's repository answered its lookup, and what is no longer public was removed. A refresh that hears from
  them all publishes it again. With `Authenticated` public reads an unanswered lookup keeps the view as before
  (ADR 0147), since the token's limit runs out rarely and the reserve stops the lookups first. A repository is public only when GitHub's answer says `"visibility": "public"` and its name is one a pin
  may name. Archives are still downloaded from codeload without any credential and without redirects, in both modes.
- **One token lifecycle, two kinds of source.** `github::Credentials` is `Command`, `gh auth token` on a thread of its
  own as before, or `Expiring`, a source of `ExpiringToken`s run as a task on the runtime within 15 s. `Tokens` holds
  one `Hold`: `Missing`, `Held` with how long the token lasts, `Reading`, or `Failed` with when to try again.
  - An expiring token is used until 5 minutes before it expires, at most an hour after it was asked for, by both the
    monotonic clock and the wall clock: a sleep moves only the wall clock, a clock set back only the monotonic one,
    and either ends it. That end goes with the token wherever it goes: a read that finishes past it is a failed read,
    a waiter handed a token that ended meanwhile gets an error, and a request checks it again after its permit. A token that is empty, holds a character a header cannot carry, or lasts no longer than the
    margin is refused, cold or on refresh alike.
  - Requests that find no usable token, or whose token GitHub refused, join the one read in flight. A refusal of a
    token no longer held changes nothing.
  - A read that fails, panics, times out, or is dropped with its runtime answers everyone waiting and ends `Reading`.
    An expiring source then waits 10 s before it is asked again, doubling with each failure to 5 minutes; a request
    meanwhile fails at once. A command's failed read is tried again by the next request, as before.
  - Nothing of it survives a restart, and a reload does not touch it.
- **A job action's permit is still asked with the token in hand.** `Github::exchange` sends once more after a 401
  and asks the permit before each send, after a refresh too. A token that ended while the permit was asked is not
  sent: a fresh one is read once and the permit asked again with it.

## Consequences

- `gh` mode sends the same requests with the same token as before.
- The App mode that follows sets `PublicReads::Anonymous`. Its archive lookups share GitHub's 60 an hour with every
  anonymous client behind the same address, and a refresh asks each pinned repository every 10 minutes, so a store
  with more than a few pinned repositories will often have lookups left unanswered: archives stay and retention holds,
  but runners get no archives until a refresh hears from every kept one's repository. The cache serves less, or not
  at all, until lookups are spaced out or the cap is reconsidered with measurements.
- An action whose repository was renamed answers anonymously with a redirect, which is not followed: its lookup stays
  unanswered, and retention holds for the repository that pins it.
- A failed refresh of an expiring token drops the token held, though it may have minutes left: no request goes out
  with a token whose refresh is known to fail.
