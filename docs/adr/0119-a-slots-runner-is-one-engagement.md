# 0119. A slot's runner is one engagement

Status: **Accepted** (2026-11-10). No wire, schema or config change.

## Context

What the engine knows of a slot's runner was six fields of `Occupant`, written from `runners`, `tick`, `history`,
`admission`, `records` and `recover`: whether the runners listing shows it busy, whether that listing was the first
to, the job a capture or the history named, when the capture found it, the job's key, and why it was started, with
`None` meaning adopted. A seventh lived beside it on `Engine`: the deadline a forced pause gives a cancelled runner,
keyed by runner name and swept each tick against the slots. A found time without a job could be written, and
"adopted" was read as "no purpose".

## Decision

- `engine/engagement.rs` holds `Engagement`, one per occupant: its origin, `Started(Purpose)` or `Adopted`; what
  the listings showed, `Idle`, `Busy` or `NewlyBusy`, one value so a runner is never newly busy and idle at once; the
  job named, with when this daemon's capture found it; the job's key; and a forced pause's cancel deadline. Its fields
  are private and only its methods write them.
- The key stays beside the job, not in it. A capture that names the job again replaces the job and keeps the key,
  which the history's record decides.
- A runners listing turns every `NewlyBusy` runner `Busy` once it is done, whether or not it answered for that
  runner. Only then does each runner it answered for take its answer. A listing that fails ends a claim as one that
  answers does.
- The cancel deadline goes with its runner's slot, so `kill_cancelled` no longer sweeps a map by name.
- An adopted runner's job counts as started here once a listing showed the runner waiting with no job named since
  its adoption (`Adoption::Waited`). A listing that shows it idle after a capture named its job does not count: the
  listing lags the capture, so the job may have started while no daemon ran.
- `Occupant` keeps what does not change once it is placed: its names, container, limits, labels, image and spec. It
  also keeps `admission`, which `rereserve` writes. Its readers delegate to `Engagement`.

A restart rebuilds the value in `adopted`. The runner counts as busy until a listing says otherwise, with the job its
history row names and no key until `find_keys` reads the job's record. Admission comes from the runner's record, not
from a Docker label. The newly busy mark, the time a job was found and a cancel deadline are lost. A reload does not
touch the value.

## Consequences

Engine has one lifecycle field fewer. An adopted runner that took its job under this daemon earns pooled credit
once its job settles, as one this daemon started does. The known gaps of an adopted runner are now explicit in one
type: it gets no claim if it takes a job before the first listing, and its cancel deadline does not outlive a
restart. Each is fixed on `Origin::Adopted` or the runner's row in its own change.
