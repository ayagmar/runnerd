# 0178. A cited log line is read where it is

Status: **Accepted** (2026-11-24). Amends [0136](0136-the-dashboard-is-laid-out-by-task.md): a job page's link to a
line opens that line however far it is from the log's end. Adds `LogWindow`, `LogOutline::window`,
`LogOutline::absent_words`, `LogOutline::unread_words` and `LogOutline::zero_line_words` to core, and the `?line=` of a job page's stream in
`crates/web`. The daemon's API and the wire are unchanged.

## Context

A finished job's page outlines the newest 5,000 lines of its log. Its search links each hit to `?line=N#LN`, each
line links to itself the same way, and the failure summary linked to its step with `#LN` alone. The page read the
same newest 5,000 lines whatever `?line=` asked, and `keys.js` opens the folds around a line only when the line is
on the page. A failure followed by more than 5,000 lines of cleanup, or a search hit before the last 5,000, linked
to a line that was not there: the reader landed on the failed step's fold at the end, or nowhere, with nothing to say
the line was not shown. A running job's tail had no line ids at all. The TUI already reads the lines around a match
it has not read (ADR 0137).

## Decision

- **Core picks the lines.** `LogOutline::window(cited, size)` is the log's last `size` lines, unless the cited line
  is before them: then `size` lines around it, about half before, started earlier at the outermost group that holds
  it when that still leaves a quarter after it, so its folds read whole. A line past the end gets the tail. The page
  reads the window through the styled-line client it already used, `size` 5,000 as before, in at most four reads.
  When long lines stop the reads short of the cited line, the page reads again from the line; a read again that
  does not reach it leaves the line absent, and the log's end shows.
- **A window says it is one.** `LogWindow::words` says `lines 1 to 5000 of 6012, around line 10, not the log's end`
  in a status notice with a link back to the end (`/jobs/{id}#log`). A window of no lines says nothing. The tail says what it always said
  (`LogOutline::tail_words`). The failure summary's link to its step carries `?line=`, as every other line link does.
- **A line not read is said, never opened.** When the lines read do not hold the cited line, the page shows the
  log's end under an alert in core's words (`absent_words`), by the log's evidence: past the end, not printed yet,
  cut at `history.log_max`, deleted after `history.logs` (whatever the line, and past the lines the log had when it
  is), no log captured, or not stored. No line is marked and no fold opens for it. `?line=0` is answered the same
  way, with `zero_line_words`: lines count from 1.
- **A cited closing line shows.** An `##[endgroup]` is no line of its own on the page, unless it is cited: then it
  shows numbered and marked, last in its group's fold, which opens, or where it is when the group starts before the
  lines read.
- **An older daemon is not missing evidence.** A daemon without the outline or its lines (`Feature::LogOutline`,
  ADR 0136; either route answering no such endpoint) cannot read around a line: the page says `log outline: not
  supported by this runnerd` as before, adds `unread_words` (`line 10 cannot be opened here: this runnerd does not
  read lines around one; the full log has what it kept`) with a link to the full log, and shows the plain tail. It
  says neither that the log has the line nor that it lacks it: the client cannot know.
- **A running job's cited line is a snapshot.** With `?line=`, a running job's log is outlined and read as a finished
  one's, said to be read once and not followed, with a link back to the live tail. The page's stream URL carries the
  line: the stream sends no log lines to it, and when the job ends it draws the finished log around the same line.
  Without `?line=` a running job's page follows its tail as before.

## Consequences

- A page with `?line=` reads up to 5,000 styled lines more than once only when the cited line is absent and the
  window was not the tail, or when long lines stopped the reads short.
- A cited `##[group]` line is marked as other lines are.
- `crates/fixtures` has `long_logged_job` (`LOG_LONG`): a failure two groups deep followed by 6,000 lines of cleanup.
  It is outside `logged_jobs`, so the parity tests that walk every logged job and expect each line on its default
  page stay as they are.
