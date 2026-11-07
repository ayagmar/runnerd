# 0102. The larger host margin stands only while no waiter fits the plain one

Status: **Accepted** (2026-11-07). Amends [0055](0055-admission-follows-the-hosts-memory.md) and
[0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md).

## Context

0055 grows the host's margin by half while any job waits for the host or the host stalls, so starts resume a little
past the edge. The wait itself held the margin up: with a queue, some job always waits, so the larger margin stood
for as long as the queue did. Jobs that fitted the plain margin waited behind a band that stood only because they
were waiting. A replay of the host's readings put 222 minutes in that band, and this rule at 15% less waiting.

## Decision

- **The larger margin stands while the host stalls, or while jobs wait for the host and none fits the plain margin.**
  `host::short` decides it, in `note_host` after the tick's last plan, from each `Waiting::HostMemory`'s `needs` and
  `host::left(host::limit(&memory, false), in_use, false)` by the plain count.
- **The room is signed.** `Waiting::HostMemory::free` is clamped to 0 and goes by the plan's margin, so it is not what
  the rule reads: a host past the plain margin fits no waiter, however little it needs.
- **A waiter between the margins starts at the next plan.** The plan that sees it holds it, the note gives the band
  up, and the next plan has it. The band comes back once no job left waiting fits the plain margin, so it changes
  only after a start or a new reading: starts and holds do not alternate on a steady reading.
- **A barrier reads the larger margin whenever a runner runs.** 0096's drained host limit was the margin in force,
  which was safe while the plain margin came back only to an empty slice. Now it comes back with runners running, and
  jobs behind a barrier are not waiters, so a barrier that followed the margin would take away the waiter that let it
  go: the two would swap at every tick and the younger job would never start. A drain's room no longer depends on the
  band. A barrier can still always end: the empty slice's plain margin is the larger room.
- **What stays.** `MARGIN_SHARE` and `MARGIN_MIN`, the stall rule, the empty slice's first runner at the plain margin,
  seeds and reseeds at the plain margin (0063), the doctor. No reading, as before, leaves no larger margin. Nothing
  that runs is ever stopped.
- **`simulate --causal` shows the band.** It replays this rule through the same `host::short`, `--any-waiter-band`
  replays the old one, and the hazards table gains the time at the larger margin. The first table gains `WAIT H`, all
  the job-hours jobs waited.

## Evidence

`runnerd simulate --causal --cpus 32 --memory-max 46G --since <window> --credit 6G` with 0099's five `--move`s, on the
2026-11-06 backup of the job history. Each cell is the old rule (`--any-waiter-band`), then this one:

| Window | All wait, job-hours | Host wait, job-hours | In the band | Lowest `MemAvailable` | Under 20G | Under 6.2G |
| ------ | ------------------- | -------------------- | ----------- | --------------------- | --------- | ---------- |
| 6h     | 80.0 → 76.1         | 27.1 → 25.0          | 2h21 → 1h39 | 7.38G → 11.59G        | 14m33 → 12m56 | 0 → 0  |
| 12h    | 111.4 → 89.3        | 44.6 → 31.3          | 3h35 → 2h28 | 12.31G → 8.7G         | 29m52 → 31m28 | 0 → 0  |
| 24h    | 111.6 → 88.5        | 44.6 → 31.3          | 3h35 → 2h28 | 12.31G → 8.7G         | 29m52 → 31m28 | 0 → 0  |
| 72h    | 126.6 → 103.2       | 59.6 → 43.7          | 6h12 → 4h37 | 12.17G → 8.66G        | 34m25 → 37m29 | 0 → 0  |

Over three days that is 18% less waiting and 27% less waiting for the host. The 12h and 24h windows hold the same
heavy night. No window reaches 6.2G or passes 90% of the slice.

## Consequences

- The host keeps up to 1.94G less room for the desktop while jobs queue: half the margin of its 62G. The owner
  accepted that.
- The lowest `MemAvailable` moved by more than that in the replay: 3.5G to 3.6G down in three windows, 4.2G up in the
  other. A changed schedule meets the host's other programs at other moments, so the lowest reading follows the
  schedule as much as the margin, and the replay cannot say what the desktop would have done with less room.
- The band still stands for three quarters of the time it did: then no waiter fits.
- The doctor's plain margin now agrees with admission more often: admission keeps the larger one only while nothing
  waiting fits the plain one, or the host stalls.
