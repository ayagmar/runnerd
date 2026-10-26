# 0074. The cost line names only what it is before

Status: **Accepted** (2026-10-26). Amends [0072](0072-job-minutes-priced-at-github-hosted-rates.md).

## Context

0072 had `SavedWindow::headline` carry the whole basis of the figure: "This month: 1,234 job-minutes here, an
estimated $7.40 at GitHub's 2-core Linux rate, before your plan's free minutes, by today's profiles and repository
visibility". On the dashboard's overview it was the longest line, under four short figures, and three lines on a
phone. Its last clause repeats the last line of `SavedWindow::details`.

## Decision

The headline keeps the figure, the rate it is at and what it comes before: "This month: 1,234 job-minutes here, about
$7.40 at GitHub's 2-core Linux rate before your plan's free minutes", with "(3 jobs not priced)" as before. That
the figure rests on today's profiles and repository visibility is said once, as the last line of `details`.

## Consequences

- The overview, `runnerctl tui`, `runnerctl insights` and the daily digest show the shorter line; `runnerctl
  insights` still prints the basis under it. The dashboard and the digest no longer state it.
- A client still shows the line whole or not at all, as 0072 says.
