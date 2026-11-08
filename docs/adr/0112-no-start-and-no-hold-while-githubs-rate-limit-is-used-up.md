# 0112. No start and no hold while GitHub's rate limit is used up

Status: **Accepted** (2026-11-08). Adds `Status.github_limited_until` to the wire; no schema or config change.

## Context

GitHub allows a token 5,000 requests an hour. When they are used up, its listings fail, so no job looks queued. Warm
targets do not read the listings, so warm starts still went ahead. Their registration then failed, and `start` held the
pool back as a registration failure and announced `runner.failed`, though nothing was wrong with the pool. A 200 with
`x-ratelimit-remaining: 0` makes it likelier: the listing answers, and the client refuses the registration after it.
Only a log line said why, once per limit.

## Decision

- **While an answer of GitHub's says its rate limit is used up, no runner starts.** The engine keeps the reset time
  (`github_limited_until`), taken from `RateLimited` answers to the runners and queued-jobs listings and to the
  registration. Until it passes, `apply` skips its starts and `refill` returns at once. Job reads that hit the limit
  do not set it: every limit that matters to a start comes through the listings or the registration.
- **A registration refused for the limit holds no pool and announces nothing.** It is no failure of the pool.
- **The status says so.** `Status.github_limited_until` carries the reset time in Unix seconds, absent while the limit
  is not used up. Core words it once, in `Status::github_line`: "GitHub's rate limit is used up: no runner starts until
  it resets". CLI status and watch, the TUI header and the web overview render that line, and nothing else. The line
  promises only that no runner starts before the reset; it gives no countdown, since the web renders with the wall
  clock and its parity fixtures use a fixed time. Clients that want the time read the field.
- **A restart forgets the limit.** It is kept in memory only, as is the GitHub client's own `blocked_until`. Until the
  first answer that refuses for the limit, the restarted daemon starts runners as usual and its status says nothing of
  it; that answer sets it again.

## Consequences

- A used-up limit costs waiting jobs time, not held pools: starts resume at the first tick after the reset.
- The field is optional and skipped when absent, and `Status` does not deny unknown fields, so older clients read the
  new status and newer ones read the old.
