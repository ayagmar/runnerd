# 0117. A runner's exit is one verdict

Status: **Accepted** (2026-11-09). No wire, schema or config change.

## Context

A runner goes in one of three ways: it ends and is released, the engine kills it, or the engine removes it as idle.
Each place decided on its own whether it had run a job, whether that job ended, and whether a listed job may be gone
with it, from locals such as an `Option<bool>` for "the log shows a job". A release has a fourth answer that a boolean
hides: when Docker gives no log, nothing is known. Counting that as a runner that connected and left would count a
Docker error toward an idle-exit hold; counting it as one that took a job would clear a broken pool's backoff.

## Decision

- `engine/runner_exit.rs` holds two values. `Logged` is what a log says: `TookJob`, `NoJob { connected }` or `Unread`.
  `Exit` is how the runner went, with only what that way knows: `Released { logged, listed_busy, job }`,
  `Killed { busy }` or `RemovedIdle { busy }`.
- `Exit::ran`, `job_ended` and `spends` answer the three questions for every way out. The engine acts on them, in
  `note_exit` and `vacate`, and spells no condition of its own.
- A pool's streaks and hold read `Logged`: `Unread` changes neither.
- `Killed` and `RemovedIdle` stay apart on purpose. A killed runner's job ended with it. A runner removed as idle, once
  GitHub agreed, had none to end, even an adopted one no listing has yet shown idle.

An `Exit` lives only as long as the step that makes it, so nothing of it survives a restart or a reload.

## Consequences

A fourth way out, or a fourth thing a log can say, is a compile error at every place that decides. Each site still
chooses when it reads what the verdict is built from.
