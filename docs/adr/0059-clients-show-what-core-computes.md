# 0059. Clients show what core computes about a job

Status: **Accepted** (2026-10-25).

## Context

The dashboard, the TUI and `runnerctl` each worked out what to show of a job from the same API types. They drifted.
A job another machine ran with conclusion `success` was `elsewhere` on the dashboard and a green `success` in the
terminal. A timed-out step was red on the dashboard and in `runnerctl`, but not in the TUI. A queued job had waited
a minute in the terminal and not at all on the dashboard. CPU time of 4600 ms read `4s` on the dashboard and `4.6s`
in the terminal. The same latest runs could have different failure counts, since each client counted failures from
its own colours.

## Decision

- `runnerd-core::api` computes what every client shows of a job, in modules that build without `host`:
  `JobSummary::outcome` and `is_failure`, `Step::outcome` and `took`, `JobSummary::waited`, `ran`, `phases` and
  `title`, `Timeline::milestones`, `Job::observe` and `reread` for a running job's peaks, `Job::compare` for its
  latest runs, `SampleSeries::usage_between` for a step's use (`api::jobs::facts`).
- Durations, ages, CPU time and cores in use have one way to be written each (`api::units`). A limit (a cap, a
  reservation) is in CPUs to a tenth rounded up (`api::cpus`); use is in cores to a hundredth rounded up.
- Core returns meaning: a label, a figure, and for an outcome its `OutcomeKind`. A client maps the kind to its own
  colour (a CSS class, an ANSI code, a ratatui style) and lays the figures out. A client never decides a fact from
  a colour or a class.
- A job fails as `GET /v1/stats` counts it: it concluded `failure` or `timed_out`, or lost its runner. A job
  another machine ran is `elsewhere` whatever its conclusion; one no runner took is worded by its conclusion
  ([0069](0069-a-job-no-runner-took-did-not-run-elsewhere.md)).
- `runnerd simulate`'s table keeps its own `4m20s`: no other client shows it, and its columns are split on spaces.

## Consequences

- A change of wording or rounding is one change, and every client follows.
- The dashboard's ages are exact (`1h 05m ago`) rather than rounded (`1 h ago`), and the terminal's durations pad
  their minutes (`2h 05m`) as the dashboard's stopwatch does.
- A client that shows a job fact must find it in core first; one that is not there is added to core, with a test
  in domain terms, not to the client.
