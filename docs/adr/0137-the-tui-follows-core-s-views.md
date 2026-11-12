# 0137. The TUI follows core's views, one row per slot, and reads what an older daemon lacks as not supported

Status: **Accepted** (2026-11-12). Amends [0022](0022-terminal-ui-is-ratatui-in-runnerctl.md) in how `runnerctl tui`
is laid out. Uses the reads of ADRs [0132](0132-one-attention-list-core-words.md),
[0133](0133-a-log-is-read-as-structure-kept-as-it-is-stored.md),
[0134](0134-a-job-action-is-previewed-then-sent-to-github-once.md) and
[0135](0135-the-job-history-is-searched-sorted-and-laid-out-by-core.md).

## Context

The TUI had three tabs (Overview, Jobs, Capacity) where core names five views, and drew each slot as a card of up to
seven rows, so a host with eight slots filled a 40-row terminal before the queue. Body text was dim as often as it was
secondary, so repository and job names read as background. The attention list, the log's outline and search, the
jobs' search, sort and timeline, the comparison with an earlier run, and cancel and re-run were in core and the
dashboard but not in the TUI. A daemon too old for one of those reads answered 404, which a client easily shows as an
empty list.

## Decision

**Tabs are core's views.** `View::tabs` maps each of `api::View::ALL` to the TUI's views, in core's order: Overview,
Jobs (History, then Records), Insights, Capacity (Sizing, then Caches) and Settings. A digit opens its tab, again its
next section. A view core adds fails to compile until it is drawn. Settings shows the doctor with the state of its
last run and the config's path and check; editing the config stays with `$EDITOR`.

**Density.** A slot is one row: its profile and number, then its job, step, run time, memory and CPU, or its state in
core's words. The overview's charts are drawn only when there are host samples and only up to their useful height;
rows nothing else wants go to the attention list. The savings line moved to the Insights view, whole or not at all.
The header shows the tabs that are not open as their digit when the row is tight, and what needs attention as a count
that opens the list.

**Contrast.** Body text is the terminal's default colour. Dim is for secondary metadata only: ages, units, column
headers, the separator. A parity test checks that repository and job names are never dim.

**The job page.** On a failed job, core's failure summary comes first. The log is rows of `LogView`: a header per step
and group that folds, lines in core's styles and colours, a match of the search reversed. `/` asks the daemon to search
the log, `n` and `N` walk the matches, wrapping. `c` swaps the log for the comparison with an earlier run. `x`, `a` and
`A` ask core for a preview of cancel, re-run and re-run failed; the preview is drawn over the page, and only `y` sends
its confirmation. Nothing is offered while `Status::job_actions` says no.

**The jobs view.** `/` searches names, workflows and branches; `f` opens a list of filters, each a list of its
values; `s` picks one of `JobOrder::ALL`, `t` one of `JobRange::ALL`; `l` swaps the table for the timeline of each
slot over the range; `x` clears every filter. The table has a record column with core's mark and tone.

**Mouse.** A click on a tab opens it, on a row selects it, on the selected row opens it; the wheel moves the
selection or scrolls the log. Drawing returns the screen's targets (`mouse::Hits`), and the key reader maps a click
through the last frame's. `--no-mouse` or a non-empty `RUNNERCTL_NO_MOUSE` leaves the mouse to the terminal, for
selecting text.

**What an older daemon lacks.** Each new read is a `Reading<T>`: waiting, read, failed with core's words, or not
supported. A route the daemon does not have (`ClientError::is_unsupported`: its 404 for no such endpoint) reads as not supported and the view says `not supported by this runnerd`
(`api::NOT_SUPPORTED`), never an empty list. A jobs list without `applied` says that search and sort are not
supported beside it.

**State.** Each new lifecycle is a value with private fields in a leaf file: `log::LogView` and its `Found`,
`overlay::Overlay` and `Picker`, `reading::Reading`. `check-state.py` passes with no new exception.

## Consequences

- The keys changed: `c`, `s` and `w` no longer cycle the history's filters; `3` is Insights and `4` Capacity. The
  site's frames and the key table in `clients.mdx` follow.
- Core gained the words a client was about to compose: `JobOrder`, `JobRange`, `RecordFilter::words`,
  `Column::Workflow` and `Column::Record`, `StyledLine::omitted_words`, `Baseline::against` and `NOT_SUPPORTED`.
- The attention list is read on `attention.changed` and every 30 seconds; a doctor's run is read when Settings opens
  or on `u`.
