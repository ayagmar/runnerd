# 0061. The previous release reads the wire

Status: **Accepted** (2026-10-25).

## Context

[0058](0058-a-repository-served-by-several-profiles.md) added `Waiting::NoPool`, which 0.0.13's clients cannot
read: their `Waiting` has no such variant and no fallback, so an older `runnerctl` or TUI fails to read the whole
status. Nothing caught it; a reviewer did. The only checks of older readers were types copied by hand into a few
tests (`api/admission/tests.rs`), for the fields someone thought of.

The daemon's JSON crosses versions in several places. An upgrade leaves an older client talking to the new daemon
until it restarts, or a new client to the daemon still running. A downgraded daemon reads the events and admissions
a newer one stored. `[notify]` webhooks parse what the daemon sends.

## Decision

- A check reads the JSON both ways between this version and the previous release, in `crates/wire-compat`:
  - The previous release reads this one's output: every sample parses as the previous release's own type, from
    `runnerd-core` pinned by commit as the dev-dependency `runnerd-core-previous`. Its real readers, not a copy or
    a schema, so serde attributes, fallbacks and tagging count as they ship. Parsing is not enough: a reader fills
    a defaulted or optional field it was not sent, and reads an unknown variant as its fallback. So what it read is
    written back and compared with the sample: every key it writes must be in a full sample, which sets every
    field, with the same value. A key that is not is a field renamed or removed; a value that differs, a fallback.
  - This version reads the previous release's output: the samples that release wrote, committed under
    `crates/wire-compat/fixtures/` with one file for every document, parse as this version's types, and written
    back they keep every key and value of a full fixture. A field skipped when empty leaves no key in what the
    previous release writes back, so only this way sees it renamed or removed.
- The samples are one Rust value per document (the responses, the events, the requests, a notification), built
  from struct literals that name every field and lists of every enum variant guarded by an exhaustive `match`. A
  new field or variant does not compile until it has a sample, so the check sees every one. Where serializing skips
  fields, a second, empty sample leaves them out.
- A break made on purpose is one line in `ACCEPTED` naming the failing item and the ADR that records it. An entry
  whose item parses fails the check, so the list stays true and empties as releases move on. The first entry is
  `waiting/no-pool`, under 0058.
- A new event type is not a break: the client's event stream and the history skip an event they cannot read. An
  event type the previous release's fixtures hold is: an older reader of a renamed or changed one, or this version
  reading one removed, would drop it silently.
- `crates/wire-compat` is a workspace of its own. In the main workspace, a second `runnerd-core` would make every
  `-p runnerd-core` ambiguous. `.github/scripts/check-wire-compat.sh` runs its fmt, clippy and tests, in the gate
  and in CI's `rust` job.
- After a release is tagged, its samples become the fixtures and the dev-dependency moves to its commit
  (CONTRIBUTING.md).

Rejected: a JSON Schema per release, diffed in CI. It needs a derive on every API type and a schema-compatibility
rule of its own, and it compares what serde says it accepts, not what it does. Fixtures alone, without the previous
release's code, check only one way.

## Consequences

- A removed or renamed field, a field newly required, a variant without a fallback in the previous release, or a
  changed representation fails CI before review.
- Query strings are checked as the same fields in JSON, not as `serde_urlencoded` reads them.
- A document type the previous release lacks is checked from the release after it. Values the samples do not hold,
  such as a field skipped when zero rather than when absent, go unchecked.
- `cargo deny` checks only the main workspace; the check's own `Cargo.lock` adds only runnerd-core's client-side
  dependencies at the previous release.
- Cutting a release has one more step, and the release's version bump updates the check's `Cargo.lock` as well.
