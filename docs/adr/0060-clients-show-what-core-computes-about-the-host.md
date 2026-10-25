# 0060. Clients show what core computes about the host

Status: **Accepted** (2026-10-25).

## Context

[ADR 0059](0059-clients-show-what-core-computes.md) moved what clients show of a job into core. What they show of
the host still drifted. The dashboard's overview scaled a running job against its profile's caps after a reload,
while the TUI and `runnerctl job` used the caps it ran with. Under caps admission both bars counted every free slot
as reserved, so "free" disagreed with the reservation printed beside it. A sizing hold was judged against the time
the advice was computed on the dashboard and against now in the TUI. `runnerctl status` called every runner that
was not running a job idle, cold or warm. `runnerd doctor` explained a valid config under measured admission in its
own words, and each client labelled doctor's checks itself.

## Decision

- `runnerd-core::api` and `runnerd-core::config` compute what every client shows of the host, in modules that build
  without `host`: the budget's lines and the parts its bar draws (`api::budget`), a slot's state and the slots'
  health (`api::slots`), what the sizing advice and auto sizing's plan mean (`api::sizing::shown`), a config
  check's verdict and budget and the blocks its preview draws (`config::budget`), insights and the digest
  (`api::insights::shown`), caches, the build cache and records (`api::storage`), the history's daily statistics
  (`api::stats`), and what an action did (`api::results`).
- Where colour carries meaning, core returns an `api::Tone`: good, warn, bad, neutral or quiet. A client maps it to
  a CSS class, an ANSI code or a ratatui style, as it maps `OutcomeKind`.
- A size that must fit a narrow block is written by `ByteSize::short`, to two significant digits, halves rounded up.
  A client drops a label that does not fit whole rather than cut a figure.
- Doctor's checks of the config and the sizing advice are built in core, so `runnerd doctor` and the dashboard's
  badge agree.

## Consequences

- The overview, the TUI and `runnerctl status` draw the same blocks from `Status::budget_parts`, and scale a job by
  the caps it ran with.
- A hold is judged against the time the client shows it, not the time the plan was made.
- A client that shows a fact about the host must find it in core first; one that is not there is added to core,
  with a test in domain terms.
