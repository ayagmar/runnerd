# 0053. A repository can let its own branches' pull requests write to the build cache

Status: **Accepted** (2026-10-24). Amends [0052](0052-shared-build-cache.md).

## Context

Under 0052 only a push, schedule or manual run of the default branch (or a `[caches.build] branches` one) writes.
So `main` reads only what the previous `main` run built, and every task the merged pull request changed misses. On
the owner's halyard/ledger, which requires pull requests to be up to date before they merge, a merge's tree is exactly
what the pull request's last run built, yet 20 of 32 `main` Android gates missed (median 197 s against 33 s for a
hit), about 70 runner-minutes a day. The owner is the only collaborator of the repositories runnerd serves and
asked for pull requests to write, per repository.

## Decision

- **Opt-in per repository**: `[[repositories]] build_cache_writers`, `"default-branch"` by default (0052 as it
  was) or `"pull-requests"`. The value names what it adds; the branches 0052 lets write still do.
- **Which pull requests.** With `"pull-requests"`, a job's uploads are also kept when GitHub says, besides what
  0052 checks (the job runs on that runner):
  - through `GET /actions/runs/{id}`: the event is `pull_request`, never `pull_request_target` or any other; the
    head repository is the repository itself; `actor`, who opened, reopened or pushed to the pull request for this
    run, and `triggering_actor`, who started or re-ran the run, are both of type `User`; and the run lists at least
    one pull request into the repository. GitHub lists none for a fork's, and may list none once a pull request is
    closed; one it lists into another repository of a fork network is left out, so it is never looked up here.
  - through `GET /pulls/{number}`, for each pull request the run lists: its `user` is of type `User`, and its head
    is the run's head branch in the repository itself.
  - through `GET /branches/{branch}` and `GET /compare/{head}...{sha}`, as 0052 does: the pull request's branch
    has the run's commit, as its head or an ancestor, by commit, at the time of the decision.
  Any lookup that fails leaves the job undecided; its uploads stay held, within 0052's limits, and are dropped if
  the job ends first. Nothing the runner's log or environment says counts.
- **Bots read.** Renovate, Dependabot, `github-actions` and any GitHub App act as `Bot`, so their pull requests and
  the runs they push or re-run only read, even on a person's pull request; so does a run a person re-ran on a bot's
  push (its `actor` is the bot). What is checked is who acted on the pull request, not who made its commits: when a
  person opens a pull request from a branch a bot or a workflow pushed, its `opened` run writes. That stays inside
  what this trusts (below): whoever can push a branch.
- Everything else of 0052 stays: per-repository entries, the uploads' limits, 200 for refused uploads, the commit
  check for branch runs.

## Poisoning

An entry's key is whatever the uploader says: the cache does not check that its content matches the inputs. A job
that may write can put any output under the key a `main` build will ask for, and so run its own code in `main`'s
build, with what `main`'s jobs are trusted with.

What this trusts: whoever can push a branch to the repository and open a pull request from it as a person, which
on a private repository means its collaborators with write access. Such a person can already run any code on the
runners through the workflows their pull request carries, and can already write the slot's own `~/.cache`, which a
later `main` job in that slot reads (0052, Consequences). The shared cache widens that reach from one slot to every
slot and past a reseed; it adds no new kind of trust. It is acceptable only where everyone with write access is
trusted with the default branch's builds, as on a repository whose only collaborator is its owner, so it stays off
unless a repository asks.

What still cannot write: a fork's pull request; a bot's pull request, push or re-run; `pull_request_target`,
`issue_comment`, `workflow_run` and every other event; a run whose commit the pull request's branch does not have,
such as one force-pushed away; a job GitHub places on another runner; any job while a lookup fails; and any
repository's jobs in another's entries. A person's token used by automation acts as that person; such automation is
trusted with what the person is.

## Consequences

On a repository that requires up-to-date pull requests, a merge to `main` finds the pull request's last run's
entries, so its build is about as fast as a hit. A pull request that pushes again finds its previous push's
entries in any slot.

A pull request's runs can also fill the cache with entries no `main` build uses; the hourly trim removes the least
recently used, so a busy repository may keep less of `main`'s.

Deciding a pull request's job takes three more GitHub requests than refusing it did, one more for each further pull
request its run lists: its pull request, its branch's head and the compare.
