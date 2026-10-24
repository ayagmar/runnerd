# 0046. Sizing says what auto sizing does next

Status: **Accepted** (2026-10-24). Builds on [0039](0039-auto-sizing.md).

## Context

With `[sizing] auto` on, the owner sees what auto sizing changed, but not whether it will change a profile again or
when: that depends on the interval since each profile's last change, `max_step`, the profile's bounds and the budget
the other changes leave, which only `sizing::auto::changes` puts together. A client working it out from the advice
would be wrong whenever a bound, the interval or another profile's change holds a profile back.

## Decision

`GET /v1/sizing` gains `auto_plan` while auto sizing is on: how often the daemon looks (`every`), `interval` and
`max_step`, the changes its next look would make (`next`, from `sizing::auto::changes` with the history, the slice's
`MemoryMax` and what runs now, as a look reads them), and the profiles a change in the last interval holds, with
when each hold ends (`held`). Working it out writes nothing and settles no pending change. It is absent while auto
sizing is off, and older clients skip it.

## Consequences

- `next` is a forecast: the history moves between now and the look, and a look writes nothing while the file on disk
  differs from the config runnerd runs with. The dashboard says so.
- `GET /v1/sizing` reads the history a second time for it when auto sizing is on, over the default 30 days whatever
  `since` the request asked for, as a look does.
- Rejected: the time of the next look (the daemon's timer is not state worth exposing; `every` bounds it), and an
  event per forecast change (clients read the plan with the page).
