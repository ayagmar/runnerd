# 0026. Commands are answered while a tick waits for GitHub

Status: **Accepted**. Amends [0019](0019-control-commands-through-the-engine.md).

## Context

A tick listed each repository's runners, then its queued jobs, one request after another, each allowed 30 s, and the
engine took commands only between ticks. With six repositories and a GitHub taking 0.3 s per request a tick took
3.6 s, and a pause waited out what was left of it; with DNS down, where musl's resolver waits 5 s per lookup, ticks
took 60 s and a pause queued behind other commands answered after 168 s. A resume skipped the engine and answered at
once, but the status and its `repository.changed` event waited for the next tick to publish them.

## Decision

- A tick lists every repository's runners at once, then every repository's queued jobs, at most four requests in
  flight (GitHub asks clients not to flood it), and reads the jobs it follows the same way. A request may take 10 s.
- While it waits for those listings, and for the job reads, the engine answers commands, publishing after each, as
  it does between ticks. Both run as tasks of their own that own what they need, so they go on while a command runs
  (a request's timeout runs on the clock), and a command can change slot state meanwhile; the tick applies the
  answers to the state the commands left: a runner a forced pause removed is deleted again (GitHub answers 404), a
  repository a reload removed is skipped, a closing job a command already closed is passed over.
- Resume is a command like pause, so its answer comes after the publish that shows it.

## Consequences

A command waits for the command before it, and for the rest of a tick: its Docker calls, and the GitHub calls tied
to them (registering a runner as it starts, deleting a runner no slot holds, cancelling a run inside a forced
pause). It no longer waits for the listings or the job reads, the part of a tick that grows with the number of
repositories and jobs. When the API answers a pause or a resume, `/v1/status` and the events already show it. A tick costs as
much GitHub quota as before; a paused repository's queued jobs are still listed, so its status keeps counting them
(an unchanged listing is a conditional request that costs no quota).
