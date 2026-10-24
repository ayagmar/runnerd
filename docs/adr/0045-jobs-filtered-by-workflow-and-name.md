# 0045. Jobs filtered by workflow and job name

Status: **Accepted** (2026-10-24).

## Context

The dashboard's history filters by workflow, and a job's page compares it with the same job's latest runs. `GET
/v1/jobs` filtered by repository, state, conclusion and time only, so a client had to read hundreds of a
repository's jobs and pick out one workflow's or one job's, and a page of 50 filtered that way could come back
short or empty while older matches existed.

## Decision

`JobQuery` gains `workflow` and `name`, each an exact match on the job's workflow name and its name in the
workflow, as GitHub reports them. The store filters on them in the same query as the others, so paging with
`before` stays exact. Both are optional and absent by default, so older clients send the same queries.

## Consequences

- A job without a workflow name matches no `workflow` filter; a `name` filter alone matches that name in every
  workflow, which the job page narrows itself.
- No index: the history keeps jobs for `history.jobs` (90 days by default), and the filters run beside the repository's index.
- Rejected: a prefix or case-insensitive match (names are copied from GitHub, never typed by hand), and a separate
  endpoint for a job's runs (one more route for what a filter says).
