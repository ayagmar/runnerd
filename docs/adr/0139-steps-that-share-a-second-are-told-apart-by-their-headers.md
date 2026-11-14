# 0139. Steps that share a second are told apart by their headers, in order

Status: **Accepted** (2026-11-14). Amends [0133](0133-a-log-is-read-as-structure-kept-as-it-is-stored.md).

## Context

[ADR 0133](0133-a-log-is-read-as-structure-kept-as-it-is-stored.md) gives a segment of the log to the one step whose
stored times hold it, and to no step when several do. GitHub stores step times in whole seconds, and a step that fails
quickly shares its second with the step before it and with every step after it: GitHub marks skipped steps and the
closing steps as starting and ending in that second. The 0.1.1 end-to-end run found every failed `test` job that way:
the build, the failing suite and the steps after it all held one second, so none of them got lines, and the failure
had no exit code or last lines. No client could open the failed step.

A group's title alone is no evidence: a step can print any title, and the review of 0133 showed a user group titled as
the next step taking that step's exit code.

## Decision

The log is cut into blocks where the runner opens a step: the first outlined line, each top-level `##[group]Run …`,
and each line the runner prints to open a step without a group (`Post job cleanup.`, `Cleaning up orphan
processes`, and now a job hook's `A job started hook has been configured …` and `A job completed hook …`). A `Run …`
group right after such a line is the same step's. Every step that ran opens with one of these but `Set up job` and the
steps the runner adds without a header: the container steps, a Docker action's `Pull …` and `Build …` steps,
`Wait for all background steps` and `Create custom image`. So each other step that ran printed one or more blocks, in
order.

A block goes to a step when every way of giving each block to a step in order gives it that step, where each way:

- gives a block only to a step whose stored times hold the block's earliest and latest time;
- gives each step that ended in `success` or `failure` at least one block; other steps, steps before the outlined
  lines of a partial log, and steps after the stored lines of a cut or growing one may have none;
- never gives a block to a skipped step.

A block that several ways give to different steps goes to no step: a user's top-level `Run …` group adds a block, and
which step printed it stays open. A block with no time constrains the order but goes to no step; its lines are a
step's only between two of that step's blocks, as before. 0133's segment rule stays where a step's start may be
missing from the blocks:

- a step named `Initialize containers`, `Stop containers`, `Wait for all background steps` or `Create custom image`;
- a top-level group titled `Pull down action image` or `Build container for action use`, which a Docker action's
  steps print (their names are not matched: a user's step can be named `Build the app`);
- an outline that only counted some groups or section lines, past its caps, so a later step's header may not be kept;
- a log no way fits.

A failed step with no lines still has no exit code or last lines, even when the log has an exit-code mark: a step that
continued on error, or a step's own output, can print the same line, and naming the wrong exit code is worse than none.

## Consequences

- The 0.1.1 run's failed jobs, and a header printed in the second the previous step ended, now get their lines, exit
  code and last lines. `crates/fixtures` has that shape as job 9407, which both clients draw.
- Outlines are assembled when read, so stored logs get the new attribution at once. The hook lines are section lines
  only for logs stored from now on; in older ones the hook's first line stays with the step before it.
- A job with `container:` or `services:`, a Docker action, or a capped outline keeps the segment rule, so its steps
  that share a second stay unattributed. So may a composite action whose inner steps print `Run …` headers in one
  second with the steps around them.
- Wrong attribution needs a headerless step the guards above do not name, and a spare block in the same second to make
  up for it: a composite action's inner header or a user's top-level `Run …` group. When in doubt the rule leaves
  lines unattributed.
