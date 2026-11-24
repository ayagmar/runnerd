# 0177. A live page says whether its data is current

Status: **Accepted** (2026-11-24). Amends [0030](0030-one-socket-client-in-core.md): a dashboard page's stream tries a
read that failed as the daemon was away again on the same stream, instead of ending. Adds `ClientError::is_outage`,
which `runnerctl` used as a private function, and `live::feed::Feed` in `crates/web`, a root of `check-state.py`.
The daemon's API and the wire are unchanged.

## Context

A live page subscribes to `/v1/events` and reads the daemon again when an event says something changed. A gap in the
events already makes the stream fail, so Datastar connects again and the page reads afresh (ADR 0030). But every
read, the first one and those after an event, ended the stream on any error: a read cut off while the daemon was busy
or restarting showed a notice and ended the page's updates for good, though the next read would have answered.

The badge in the header was Datastar's request indicator: **Live** while the request was open, whatever it delivered,
and **Offline**, titled "Not connected; retrying", once it closed, including when the stream had ended for good
because the job finished, the browser was signed out or the daemon refused the read. Nothing said how old the data
on the page was.

## Decision

- **An outage is retried on the same stream.** `ClientError::is_outage` is core's: nothing listens on the socket, the
  machine cannot be reached, the connection failed, or the daemon did not answer in time. Every read of a live page
  (its event subscription, its first read, and its reads of the status, the running and recent jobs, attention, the
  day's figures and a job) goes through `live::read!`, which tries an outage again after a wait that starts at
  0.5 s and doubles to 10 s, Datastar's own longest wait. The page keeps what it shows; the stream keeps its sent
  regions and a job page its next log line, so nothing is sent twice and nothing is lost. An error the daemon
  answers, a 404 or a 500, still ends the stream with a notice: a retry would most likely fail the same way.
- **One owner says what the page's data is.** `Feed`, in the leaf module `live/feed.rs` (ADR 0114), is awaiting the
  first delivery, current, retrying after a number of failures and why, or ended for good: finished, signed out,
  refused or stopped. It holds when the page's data is from, the page's render or the stream's last delivery, and
  only `Sent::update`, which sends what the page lacks after the daemon was read, sets it; a keep-alive never does.
  The stream sends the `_feed` signal when it connects, with each retry, with each delivery and when it ends.
- **The badge shows the feed, and what only the page knows.** The stream's state and words come from `Feed`. While
  Datastar's request is not open the badge says **Offline** and that it connects again, which Datastar does, unless
  the feed ended, which it then shows: **Finished** with no action, **Signed out**, **Not updating** or **Stopped**
  with **Reload**, which leads a signed-out browser to the page that says how to sign in. Whenever the page is not
  current, the badge shows the time its data is from.
- **Sign-out and stopping win over a retry.** `run_page` races the page's run, retries included, against the
  session's sign-out, and the retry's wait ends when the page closes or the dashboard stops.

## Consequences

- A daemon that is away while its event stream stays open leaves the page on **Retrying** with its last data until a
  read answers. One whose event stream ends still makes the page connect again, as before.
- Datastar's own retries of a failed request are unchanged; the session, cookie, CSP and the socket path are too.
- The fake daemon cuts off the next reads of a path on request (`Fixtures::drops`), and the demo does so every
  `RUNNERD_DEMO_DROP_EVERY` seconds and finishes its running jobs after `RUNNERD_DEMO_FINISH_AFTER`, for the browser
  test.
