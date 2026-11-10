# 0102. The larger host margin stands only while no waiter fits the plain one

Status: **Accepted** (2026-11-07), amended by [0108](0108-the-host-limit-counts-what-runs-uses-and-keeps-room-for-its-two-largest-growths.md). Amends [0055](0055-admission-follows-the-hosts-memory.md) and
[0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md).

## Context

0055 grows the host's margin by half while any job waits for the host or the host stalls, so starts resume a little
past the edge. The wait itself held the margin up: with a queue, some job always waits, so the larger margin stood
for as long as the queue did. Jobs that fitted the plain margin waited behind a band that stood only because they
were waiting. A replay of the host's readings put 222 minutes in that band, and this rule at 15% less waiting.

## Decision

- **The larger margin stands while the host stalls, or while jobs wait for the host and none fits the plain margin.**
  `host::short` decides it, in `note_host` after the tick's last plan, from each `Waiting::HostMemory`'s `needs` and
  `host::left(host::limit(&memory, false), in_use, false)` by the plain count. A job that plan starts, whose start the
  tick did not make, such as one that failed without a backoff, counts as a waiter by its runner's memory: it fits,
  and the larger margin would hold it back at the next plan.
- **The room is signed.** `Waiting::HostMemory::free` is clamped to 0 and goes by the plan's margin, so it is not what
  the rule reads: a host past the plain margin fits no waiter, however little it needs.
- **A waiter between the margins starts at the next plan.** The plan that sees it holds it, the note gives the band
  up, and the next plan has it. The band comes back once no job left waiting fits the plain margin, so it changes
  only after a start or a new reading: starts and holds do not alternate on a steady reading.
- **A barrier reads the larger margin whenever a runner runs.** 0096's drained host limit was the margin in force,
  which was safe while the plain margin came back only to an empty slice. Now it comes back with runners running, and
  jobs behind a barrier are not waiters, so a barrier that followed the margin would take away the waiter that let it
  go: the two would swap at every tick and the younger job would never start. A drain's room no longer depends on the
  band. A barrier can still always end: the empty slice's plain margin is the larger room. An owner the host does not
  hold reads the plain margin, as its wait does not follow the band.
- **What stays.** `MARGIN_SHARE` and `MARGIN_MIN`, the stall rule, the empty slice's first runner at the plain margin,
  seeds and reseeds at the plain margin (0063), the doctor. No reading, as before, leaves no larger margin. Nothing
  that runs is ever stopped.
- **`simulate --causal` shows the band.** It replays this rule through the same `host::short`, with what the daemon
  gives it: a job behind a barrier is no waiter there, its owner is. `--any-waiter-band` replays the old rule, the
  hazards table gains the time at the larger margin, and the first table gains `WAIT H`, all the job-hours jobs waited.

## Evidence

`runnerd simulate --causal --cpus 32 --memory-max 46G --since <window> --credit 6G` with 0099's five `--move`s, on the
2026-11-06 backup of the job history. Each cell is the old rule (`--any-waiter-band`), then this one:

| Window | All wait, job-hours | Host wait, job-hours | In the band | Lowest `MemAvailable` | Under 20G | Under 6.2G |
| ------ | ------------------- | -------------------- | ----------- | --------------------- | --------- | ---------- |
| 6h     | 80.6 → 76.7         | 27.1 → 25.0          | 2h21 → 1h39 | 7.38G → 11.59G        | 14m33 → 12m56 | 0 → 0  |
| 12h    | 111.6 → 92.4        | 44.6 → 33.0          | 3h35 → 2h27 | 12.31G → 9.69G        | 29m52 → 32m21 | 0 → 0  |
| 24h    | 111.6 → 92.4        | 44.6 → 33.0          | 3h35 → 2h27 | 12.31G → 9.69G        | 29m52 → 32m21 | 0 → 0  |
| 72h    | 126.6 → 105.8       | 59.6 → 46.3          | 6h12 → 4h38 | 12.17G → 6.56G        | 34m25 → 36m00 | 0 → 0  |

The 12h and 24h windows hold the same heavy night. No window reaches 6.2G or passes 90% of the slice.

The saving depends on when the queue meets the host's other programs. Over 28 alignments of the 72h window (`--shift`,
none to 13h) this rule waits between 18% less and 6% more, 7% less at the median and 16% less as recorded; host wait
falls by a median of 21%. It waits more at one alignment, `--shift 40m`, 112.8 to 119.1 job-hours.

## Consequences

- **A start between the margins can cost one job's use at a later burst.** 1.94G, half the margin of a 62G host, bounds
  what a start is checked against, not what `MemAvailable` loses afterwards. The job brings its whole size, and the
  host's other programs can burst on top of it. The rule lets the job in; the schedule decides whether the burst comes.
  The owner accepted 1.94G, and the worst moment as recorded is larger: over 72h the lowest reading falls 5.6G, to
  6.56G, 0.36G above the 6.2G line.
- Over the 28 alignments the lowest reading changes by a median of -0.04G (mean -0.41G, from -5.61G to +4.07G): lower in
  15, higher in 10, equal in 3. Each rule goes under 6.2G in 2 of the 28: this one for 6 s at 2.89G and 18 s at 5.31G,
  the old one for 2 s at 6.09G and 16 s at 5.62G. Neither passes 90% of the slice.
- Time under 20G is the steady cost: longer in 20 of the 28, by a median of 1m57, 88 minutes in all.
- The band still stands for three quarters of the time it did: then no waiter fits.
- The doctor's plain margin now agrees with admission more often: admission keeps the larger one only while nothing
  waiting fits the plain one, or the host stalls.
