# 0133. A log is read as structure, kept as it is stored

Status: **Accepted** (2026-11-12). Adds `GET /v1/jobs/{id}/log/outline`, `.../log/lines` and `.../log/search`, the
types in `api::jobs::log`, and schema 22. `GET /v1/jobs/{id}/logs` and the `log.line` event are unchanged.

## Context

Both clients draw a job's log as the plain text the daemon stores: escape codes and all, no folding, no search, and
nothing that says which step printed what. A failed job's page should open at its failure. Three things stand in the
way:

- The history stores lines, not steps. The runner writes one job log; GitHub reports steps separately, with times in
  whole seconds (ADR 0018). `##[group]` is no step boundary: user groups nest, set-up and post steps print without one,
  and skipped steps print nothing. `record::banner` matches steps to `Run …` groups, then by exit code, then by
  position, which is good enough for a shell's first lines and wrong as an authority.
- A log may be 50 MiB (`history.log_max`). Parsing it on each request multiplies across clients and holds the store's
  one reader.
- What is stored is not always the whole log: it may be cut at the cap, partly or wholly pruned, kept by a runnerd
  that read no structure, or begun before an upgrade.

## Decision

**Outline as it is stored.** The writer feeds each line it stores to an `api::Outliner` and keeps what it returns in
the same transaction (`store::outline`): groups with their `##[group]` and `##[endgroup]` lines, error and warning
lines, the runner's `Post job cleanup.` and `Cleaning up orphan processes` lines, and counts. Its state between batches
is one row per job (`log_outlines`) and the open groups (`log_groups` with no end), so a restart resumes exactly where
the stored lines end. Nothing lives in memory between batches, and only the writer and pruning write these tables.
Bounds: groups nest 8 deep, 1000 groups and 1000 error and warning marks a job; past them the outline counts and says
`capped`. An `##[endgroup]` closes a group not kept before a kept one, so the kept ranges stay right.

**Steps by evidence.** The stored steps stay the authority on a step's times and outcome. Which lines a step printed is
best effort, worked out on read: a segment starts at the first outlined line, at each group and at each of those two
runner lines; it goes to the one step, at or after the previous segment's, whose times hold its first and last line's
times, narrowed by a `Run …` title equal to the step's name when several do. A segment between two of one step's is
that step's. Anything else is unattributed, never placed by position. A step reads `lines`, `empty` (skipped or not
started) or `unattributed`, and the outline lists the stored lines no step got.

**Evidence.** `LogEvidence` says, in this order, `missing` (no line stored), `expired` (its first or last stored line
is gone), `not-outlined` (stored before schema 22), `truncated` (cut at `history.log_max`), `partial` (outlined from a
later line, for a job that ran across the upgrade), `running` or `complete`. Core words each and gives its tone.

**A failure from what is there.** On a failed job the outline carries a `FailureSummary`: the first step GitHub reports
failed, the exit code the runner printed in that step's lines, the step's last 10 lines, and `runnerctl shell N` while
its record is kept (`RecordNote::shell_command`). Each part is absent when its evidence is: no exit code or lines for a
step whose lines are unattributed, cut off or pruned. `FailureSummary::headline` words it.

**Styled lines.** `StyledLine::parse` splits off the runner's timestamp, classifies `##[group]`, `##[endgroup]`,
`##[error]`, `##[warning]`, `##[notice]`, `##[debug]` and `##[command]`, maps SGR to seven colours and bold, and
drops every other escape and control character but a tab. Line numbers are the stored ones. No escape reaches a client
through these routes; the plain log and `log.line` still carry the text as stored.

**Routes.**

- `GET /v1/jobs/{id}/log/outline`: the outline, JSON. Reads the outline's rows and, per segment, two lines; for a
  failed step, at most its last 200 lines.
- `GET /v1/jobs/{id}/log/lines?from&limit&follow`: `application/x-ndjson`, one `StyledLine` per line. Without `follow`,
  `limit` lines (1000 by default, 10,000 at most); with it, streamed until the job is final, as `/logs` streams.
- `GET /v1/jobs/{id}/log/search?q&from&limit&context`: a literal query of 1 to 200 bytes whose ASCII letters match in
  either case, against the styled text. Up to 50 matches (200 at most) with up to 5 lines around each, their runs split
  so the match is marked. One request scans at most 50,000 lines or 4 MiB, in batches of 500 lines, each its own read,
  and answers `next` where the next page starts. `follow=true` answers 400: a client searches again as a log grows.

A client reads a route the daemon answers `404 no such endpoint` for as not supported (`ClientError::is_unsupported`),
never as an empty log. New enums read an unknown value as `Unknown`.

## Consequences

- Schema 22 adds three tables and an index. One-way: 0.1.0 refuses a history at 22, as it refuses any newer one
  (ADR 0073). Logs stored before it are not backfilled; they read as `not-outlined`, lines and search intact.
- Pruning deletes a job's outline right after its lines; a prune stopped between the two leaves an outline that reads
  `expired`, which the next prune deletes.
- Storing a line costs a classification of its prefix, and a row for a group, mark or section line.
- Attribution fails where the log cannot tell: a header in the second the previous step ended, under a custom step
  name, stays unattributed. Exact step-to-line attribution needs the runner to say which step wrote each line, a change
  to the capture this decision does not make.
- `record::banner` and `record::runner_fault` keep their own reading of the log; moving them onto the outline is a
  separate change.
