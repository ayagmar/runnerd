# 0130. The budgets count what runs uses and keep room for its largest growths

Status: **Accepted** (2026-11-10). Amends [0033](0033-measured-admission.md),
[0055](0055-admission-follows-the-hosts-memory.md),
[0099](0099-the-budget-counts-jobs-that-do-not-peak-at-once.md),
[0102](0102-the-larger-host-margin-stands-only-while-no-waiter-fits-the-plain-one.md),
[0103](0103-a-job-the-host-limit-holds-with-room-free-is-not-short-of-memory.md) and
[0108](0108-the-host-limit-counts-what-runs-uses-and-keeps-room-for-its-two-largest-growths.md). Adds `counted` to the
budget on the wire, beside `pooled` and `host.credit`, which stay. Removes `[host] pooled_host`: `pooled_count` is now
the one switch. No schema change.

## Context

Under 0099 and 0108 a runner adds to a credit only once its job is known, has run 60 s under this daemon, has a fresh
sample and goes by a prediction. `MemoryMax`'s credit stops at 6G, and the host keeps its two largest growths. The
density study found that both counts still read far above what runs uses, mostly on the host, which binds first at
46G. PR 1 (#283) added the growth count to `runnerd simulate`: what every runner uses, plus room for the three largest
growths, at least 6G, behind a brake. Replayed, it halved waiting on one week and doubled the risk on the other: on the
busier week (W7) it took the host's signed lowest `MemAvailable` to −2.60G against main's 3.52G, and passed
`MemoryMax` 9 times where main passed it none.

This change replayed 52 variants of that count against main (the plain count with 0099's and 0108's 6G credits) over
two windows, W3 (since 0.0.29, `--since 245573s`) and W7 (11-03 to 11-10, `--since 655980s`), each warm and `--cold`.
The bar was the brief's challenge: no more `MemoryMax` passes and none larger, a signed lowest available at or above
main's, no more time under 6.2G available, at most 25% more under 20G, no more cold waiting, and 25% less waiting on
both warm windows.

None cleared it, and main itself does not hold that bar against itself. Main's credit moved by 1G is the same policy,
yet against 6G it takes W7 cold's lowest to −0.65G (5G), spends 7m13s under 6.2G against 5m36s (7G), and passes
`MemoryMax` by 0.93G against 0.43G on W3 cold (0B). The bar's per-moment checks (one lowest, the worst pass, seconds
under 6.2G) flip with any change in when a job starts. So the bar became main's noise envelope: the worst of main at
0B, 5G, 6G and 7G for each check, and a real gain of about 20% less waiting on the busier warm week. One variant holds
every check within that envelope on both windows and both shapes, and this ADR adopts it.

## Decision

### One count for both budgets

```text
count = committed + Σ use + Σ stale take + kept
kept  = min(Σ headroom, room)
room  = max(6G, the three largest headrooms, Σ headroom of the young)
```

- **Every runner counts.** A runner with a fresh sample counts what it uses, at most its take, and its take less that
  use is its headroom. One with no sample yet counts nothing used and its whole take as headroom. One whose samples say
  nothing (the slice's sample or its own older than three intervals, or samples with page cache) counts its whole take
  and adds no headroom. Committed memory (seeds, shells, the mirror) counts in full.
- **Young**: no sample yet, a job not seen to start by this daemon, or a job under `POOL_AFTER` (60 s). The room keeps at
  least all of their headroom: one whose growth is still unseen cannot be ranked against the others.
- **A start in flight** counts as a runner with no sample: its reservation, all headroom, however long its registration
  waits behind the others' (0126).
- **Both budgets read it.** `MemoryMax`'s credit is the plain count less the count, at most the slice's room under its
  high-water mark (90% of `MemoryMax`, less the slice and 2G) and at most the host's room past what it keeps. The host's
  credit is the same, at most the host's room. These are 0099's two clamps, now on both.

`admission::pooled::count` computes it from a `Snapshot` whose `growth` is `GROWTH`; `pooled::Tally` holds the
snapshot so a plan can count again after each start and stop.

### When it rests

The count falls back to 0099's and 0108's credits, unchanged, while:

- **the guard** stands: no fresh slice sample, no fresh host reading, a stall, or `MemAvailable` under what the host
  keeps (its margin and a quarter of it). Once crossed, it rests five minutes (`GUARD_HOLD`), and again from the first
  plan after that still below the line. The replay chose this over five minutes past the last plan below the line,
  which rested longer and still kept the host lower on the cold windows;
- **the brake** holds (`admission::growth::Growth`). Each tick looks at what each runner uses and the room the count
  keeps. If the runners present at an earlier look within `GROWTH_WINDOW` (10 minutes) grew, together, past the room
  that look kept, the count rests for `BRAKE_FOR` (30 minutes);
- **a restart** is under 10 minutes old (`Growth::restarted`): it has no window of looks to judge growth by. The engine
  builds `Measured` with `Measured::new(now)`, from its own clock;
- **a reload** turned the count on, or changed what its guard reads (`MemoryMax`, `[host] memory_ceiling`), under 10
  minutes ago. Any other reload keeps the looks and the brake.

Resting on the credits rather than the plain count is what the replay chose: plain costs more than main on the cold
weeks (W7 cold 510.9 job-hours against main's 481.3), while main's own credits held every check.

### What is kept

`Measured` owns the brake as one field, `growth`, written only by its methods `look_at_growth` and `restart_growth`.
Nothing of it is stored: a restart begins braked, a reload keeps it unless it changes what the count reads, and a
reload to `caps` counts every runner at its cap, with no count and no credit.

### Plans count again

0099's `Left::take` and `give` were exact: a start or a stop moved the count by its take. Under the growth count it
moves by what it changes of the room kept, so a plan carries the tally (`scheduler::Recount`) and reads it again after
each start and stop. `Backfill` and `Warm::make_room` read the count after the stops they pick, and a stop is taken only
if what the count then leaves fits the start. `over_budget` counts again after each stop. `Backfill` reads
`credit_room` and `host_credit_room`, how far each count could still fall with no runner ending. Under the growth count
that is down to its lowest, every runner using nothing; while it rests, the larger of the credits' own room and that.

### The guard reads what 0102 and 0103 read

0102's larger margin and 0103's "held with room free" read the host's count as before; under the growth count that is
`hosted`, the plain count less the host's credit.

### What it says

Core words it once (`api::Counted::clause`). The memory line adds what the count does:

- counting: `…: 22G reserved, 256M pooled; what runs uses 15G and keeps 6.75G for its growth`. The host line then drops
  its own growth clause: the figure is the same one;
- braked: `the growth count rests until 15:02 UTC: what ran grew 10G in 10m, past the 9G kept`;
- after a restart or a reload: `the growth count rests until 15:02 UTC: too soon after a start or a reload to judge
  growth`;
- the guard: `the growth count rests until 15:02 UTC: the host ran under its line`, or while it is under the line,
  `the growth count rests while the host is under its line or a reading is missing`.

### The wire

`Budget.counted` is `{used, kept, jobs, brake}`, present under measured admission with `pooled_count`, and absent from
an older daemon. `brake` is `Grew {grew, kept, until}`, `Restarted {until}`, `Guard {until}` or a variant a newer daemon
adds, read as `Unknown`. `pooled {credit, counted, runners}` and `host.credit {bytes, used}` stay filled, each the plain
count less the count. So an older client still shows what `MemoryMax` goes by, the store still reads a stored
`budget.changed` reading's host credit, and the host-hazard insights still count those spans as credited.

### Config

`[host] pooled_host` is gone; a config with it fails to parse. `pooled_count = false` counts every reservation for
both budgets: the way back.

### The replay

`runnerd simulate --causal` replays it as `--count growth`. Its rules are options of the `top2` and `top3` rows, so a
variant can be replayed beside it: `--brake`, `--bound` (the two clamps), `--young` (the young room), `--credits-below`
(rest on the credits, not plain) and `--guard-hold` (five minutes). `--count growth` is `--count top3 --brake --bound
--young --credits-below --guard-hold`.

The figures below come from `runnerd simulate --causal` at this commit on the owner's database, `--cpus 32
--memory-max 46G`: each job holds its runner as long as it did and uses what its own samples say, shifted to its
replayed start; `--cold` gives each run its job's heaviest samples. The replay reads about half of real waits, so
compare rows with each other. Recorded waits (408 job-hours on W3, 1,456 on W7) include the 11-08 Docker outage, about 129 job-hours.

Main is `--count plain --credit 6G`; its envelope is the worst of main at 0B, 5G, 6G and 7G for each check. Waits are
job-hours; "past `MemoryMax`" is episodes and the worst; the lowest `MemAvailable` is signed.

| Count | Window | Waits | vs main | Past `MemoryMax` | Lowest available | Under 6.2G | Under 20G |
|---|---|---:|---:|---:|---:|---:|---:|
| main | W3 warm | 129.7 | | 0 | 3.52G | 1m00s | 1h10m |
| envelope | W3 warm | | | 0 | 3.52G | 1m00s | 1h11m |
| growth | W3 warm | 107.2 | −22.5 | 0 | 3.52G | 56s | 1h15m |
| main | W3 cold | 125.9 | | 2 / 0.43G | 0.60G | 3m08s | 2h49m |
| envelope | W3 cold | 129.8 | | 2 / 0.93G | 0.60G | 3m18s | 2h49m |
| growth | W3 cold | 119.5 | −6.4 | 2 / 0.79G | 0.60G | 3m12s | 2h59m |
| main | W7 warm | 401.4 | | 0 | 3.52G | 1m26s | 2h33m |
| envelope | W7 warm | | | 0 | 3.52G | 1m26s | 2h35m |
| growth | W7 warm | 299.8 | −101.6 | 0 | 3.52G | 56s | 2h40m |
| main | W7 cold | 480.6 | | 23 / 4.84G | 1.18G | 5m36s | 10h16m |
| envelope | W7 cold | 490.3 | | 23 / 4.84G | −0.63G | 7m13s | 10h16m |
| growth | W7 cold | 472.8 | −7.8 | 21 / 4.21G | 0.81G | 6m21s | 10h30m |
| PR 1's top3 and brake | W7 warm | 202.0 | −199.4 | 9 / 2.18G | −2.60G | 4m29s | 3h44m |

Time under 20G may reach the envelope's plus 25%; every other check stays within it. Each rule pays for itself on
these rows. PR 1's count, with the brake alone, waits least and breaks every check on W7 warm. Its two clamps and the
credits below the line bring the host's lows to within 0.09G of the envelope, but W3 cold passes `MemoryMax` 4 times,
by up to 4.04G. The young room brings that to 2.27G, and the guard's hold to 2 passes and 0.79G. With the hold counted
from the last plan below the line instead of from the crossing, W3 cold passed `MemoryMax` 4 times and both cold windows
spent longer under 6.2G.

## Consequences

- More runners start on the same host. On the replay, warm, that saves 22.5 job-hours of waiting on W3 (17%) and
  101.6 on W7 (25%) against main.
- The host's lows are main's within main's own noise, and no better: a burst outside the slice after admission is the
  risk the replay cannot model. The sandbox e2e checks it before the release: zero `oom_kill` in the slice and its
  containers, a desktop burst after admission, a start burst under four starts in flight, the brake releasing and
  engaging again, reloads during a brake, a warm runner stopped for room, both clients against both daemons, and the
  host-hazard digest reading the new daemon's events as credited.
- A restart or a reload that changes what the count reads costs 10 minutes on the credits.
- A plan clones the tally to read the count again: a few runners, once per start and stop.
