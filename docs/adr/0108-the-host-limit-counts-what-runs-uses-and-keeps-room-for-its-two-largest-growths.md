# 0108. The host limit counts what runs uses and keeps room for its two largest growths

Status: **Proposed** (2026-11-07). Amends [0051](0051-insights-and-notifications.md),
[0055](0055-admission-follows-the-hosts-memory.md),
[0099](0099-the-budget-counts-jobs-that-do-not-peak-at-once.md) (its section "The host reads the plain count"),
[0102](0102-the-larger-host-margin-stands-only-while-no-waiter-fits-the-plain-one.md) and
[0103](0103-a-job-the-host-limit-holds-with-room-free-is-not-short-of-memory.md). Adds `[host] pooled_host`, on by
default, an optional `credit` to the budget's `host` on the wire, a `host-low` insight kind, and host figures in
insights and the digest. No schema change.

## Context

At 46G the host binds before `MemoryMax`. In a 39-hour window of the owner's history, some job was held for memory past
30 s for 10.3 hours, and 94% of that time the host's limit was what held it. The plain count read a median of 35G there
while the slice used 14G. 0099 kept that overcount as the desktop's buffer: a runner let in cannot be taken back, and
the host study saw 10G of desktop memory arrive in 43 s.

The owner has since decided that CI may spend the idle part of that buffer, as long as the cost is reported. What is
left to decide is how much to keep. Runner samples from 11-01 19:23 to 11-07 18:45 (65.6 hours with a runner running)
show how much what runs can grow on its own. G(d) is the largest rise within d in the summed memory of the runners
running; eligible runners are those 0099's credit counts:

| Window d | All runners: p99 / p99.9 / worst | Eligible only: p99 / p99.9 / worst |
|---|---|---|
| 1 min | 8.1 / 11.3 / 14.2G | 3.8 / 6.1 / 9.0G |
| 5 min | 10.8 / 15.3 / 19.7G | 7.3 / 12.2 / 15.8G |
| 10 min | 11.2 / 16.0 / 19.9G | 7.8 / 13.5 / 16.0G |

Over 10 minutes eligible runners realise a median of 14% of their summed headroom, p99 61%. Correlated growth happens,
but it rarely reaches more than two jobs' worth. Each candidate reserve over the same 65 hours, at the 14,008 moments
with an eligible runner:

| Reserve kept for growth | 10 min: exceeded (beyond the summed headroom left out) / worst | Mean freed |
|---|---|---|
| Sum of each job's headroom (the plain count) | 0.11% (0) / 5.4G | 0 |
| Fixed 6G | 3.51% (3.39%) / 10.0G | 3.2G |
| Largest headroom, at least 6G | 0.73% (0.61%) / 5.4G | 2.5G |
| Root-sum-square of headroom, at least 6G | 0.27% (0.16%) / 5.4G | 2.2G |
| **Two largest headrooms, at least 6G** | **0.15% (0.04%) / 5.4G** | **1.5G** |

A fixed reserve is exceeded far more often than one taken from what runs, and a sum is the plain count.

## Decision

### The host counts what eligible runners use, plus their two largest growths

```text
H_job        = sum over the job's eligible runners of take - use          (as 0099)
host_credit  = max(0, sum H - max(6G, H_1 + H_2))                          (H_1, H_2: the two largest H_job)
hosted       = plain - host_credit
             = ineligible takes + committed + eligible use + min(sum H, max(6G, H_1 + H_2))
```

`admission::pooled::count` computes it beside 0099's credit, from the same eligible runners: job known, run 60 s under
this daemon, a fresh sample without page cache, basis `Job`, `Profile` or `ProfileMax`. `HOST_RESERVE_JOBS = 2` and
`HOST_RESERVE_FLOOR = 6G` are its constants. Everything else counts at its take, as in 0099: waiting and warm runners,
young, adopted, overrun and capped ones, the mirror, seeds and shells. With one or two eligible jobs the floor covers
their headroom, so a host credit needs at least three.

- **What reads it.** The host's limit, `Left.host`, the last check before a start, `host::short` (so 0102's band), a
  requested reseed's check of the host, and `simulate --causal` read `hosted`. `MemoryMax`, `over_budget`, reseeds and
  shells keep reading 0099's `pooled()` against `MemoryMax`. A reseed reads the host as plans do, or plans would keep
  spending the room it waits for.
- **When none stands.** No host credit while the host stalls, without a fresh slice sample or host reading, under
  `caps`, or with `[host] pooled_host = false`. `pooled_count` and `pooled_host` are separate switches. A slice with no
  `MemoryMax` gets the host credit all the same: there the host's limit is the only bound, and the slice sample is read
  for its freshness alone. 0099's credit needs `MemoryMax` and stays 0 there.
- **0099's `host_room` guard stays.** It lowers only the `MemoryMax` credit when the host nears its lower line; the host
  credit does not read it. The study replayed the same guard on the host credit: it kept the host's lows at today's
  level and gave back half the gain, which is the trade the owner declined.
- **After a restart.** Adopted runners have no start this daemon saw, so they are never eligible. A restart takes the
  host credit back at its first tick, and the host's limit reads the plain count until they end.

### Barriers

`Drained` is unchanged: an empty slice's limit less committed memory, so a barrier can always end, whatever the credit
does. `Left.host` reads `hosted`.

The host credit has no cap, and it can grow with no runner ending: a runner passes 60 s, or shrinks and raises its own
headroom. So `Backfill` adds `host_credit_room` to `left.host` when it works out when room gathers:

```text
host_credit_room = max(0, sum of the running runners' takes - 6G - host_credit now)
```

Every job's headroom is at most its take, so this bounds what the credit can still give without a runner ending, and
0097's first promise holds as 0099 restored it. A runner the same plan started is not in that sum, and needs no room of
its own: it has no job yet, so `Backfill` counts it as ending now and gives its whole take back at once, and what it may
later add to the credit is its headroom, never more than that take. A bound by `take - use` instead would read today's use, which a
shrinking runner beats. Replayed at 46G over 72 hours, 28 alignments, the strict room costs nothing measurable against
none: all waiting 107.35 → 107.3 h warm and 111.85 → 113.2 h cold (the paired median change is 0.0 h and 0.1 h), host
hours 27.75 and 27.5 either way.

### What it says

Core words the host credit once; every client lays it out.

- **The host line** adds what runs counts there: `host: 8.5G of 64G available, swap 11.5G of 16G used; leaves the slice
  22.5G of its 24G, so 768M is free; what runs counts 21.75G: it uses 15G and keeps 6.75G for growth`. A limit past
  `MemoryMax` reads `47.5G for the 40G counted there`.
- **A job held with room free** (0103) says what it waits for: `The host has 30G of 62G available, but what runs may
  still grow into it: it uses 14G and keeps 9G for growth; needs 7G, 2G is free`. `keeps … for growth` is `hosted`
  less eligible use: the two largest growths or the 6G floor, every runner too young or unknown to measure at its take,
  and committed memory. It is one figure, as the host line gives it.
- **The log** says `what runs counts … of the … the host leaves the slice, with … available: it uses … and keeps … for
  growth` while a credit stands, and `what runs reserves …` otherwise.
- **The wire.** `HostBudget.credit` carries `bytes` (the host credit) and `used` (what eligible runners use), and is
  left out when no credit stands. `budget.changed` carries it, and fires when only the credit changes, as for any change
  of the budget.

### The cost is reported

From the stored `budget.changed` readings, while a runner ran, each reading counting until the next for at most a
minute:

- **Per UTC day**, in `runnerctl insights` and on the dashboard's insights page, and **over the last 24 hours** in the
  digest: how long CI ran and how much of it on a host credit; the time with `MemAvailable` under the low line (margin
  plus a quarter of the host, 19.4G of 62G) and under a tenth of the host (6.2G of 62G), each with its share on a
  credit; the lowest `MemAvailable`, and on a credit; the lowest `SwapFree`.
- **A `host-low` warning insight** for the latest day the host fell under a tenth of its memory while a host credit
  stood. Its key is the day's, so the existing insight notifications tell it once a day at most. It names
  `[host] pooled_host = false` as the way back.

No weekly notice: the digest and insights already compare. No new chart until the figures have a week of data.

## Consequences

Replayed with the shipped `runnerd simulate --causal --cpus 32 --memory-max 46G --since 72h --credit 6G`, the five
fretboard `--move`s of 0099, `--host-reserve plain` against `top2`, and `--shift` 0 to 13h30 every 30 minutes (28
alignments of the desktop's readings), on the history copied at 2026-11-07 18:45 UTC:

| 46G, 72 h | All wait h, median (range) | Host-held h, median | Under 20G, minutes summed | Under 6.2G: alignments, seconds summed | Lowest `MemAvailable`, median / worst |
|---|---|---|---|---|---|
| Warm, plain | 118.0 (102.6–160.1) | 43.5 | 1,458 | 7, 540 | 7.00G / 0B |
| Warm, top-2 | 107.3 (98.3–134.0) | 27.8 | 1,708 | 11, 576 | 6.53G / 0B |
| Cold, plain | 123.9 (107.5–166.2) | 41.2 | 4,069 | 21, 1,483 | 4.08G / 0B |
| Cold, top-2 | 113.2 (102.2–140.8) | 27.5 | 4,540 | 23, 1,587 | 3.82G / 0B |

Paired at each alignment, top-2 cuts all waiting by 8.8% warm (4.0 to 16.5%) and 8.2% cold (2.6 to 17.0%). The lowest
`MemAvailable` moves by a median of −0.04G warm (−5.57 to +3.83G) and 0.00G cold (−3.41 to +2.67G). Time under 20G
grows by about 9 minutes per 72 hours warm and 17 cold. The host credit comes to a median of 33 GB-hours warm and 36
cold. At 32G `MemoryMax` binds and almost nothing changes. The worst column reads 0B because `simulate` floors the
reading; the study's signed figures were −2.98G and −0.16G warm, −2.28G cold either way.

- **A desktop burst.** A 10G burst in 43 s at fixed moments (two alignments, 0m and 360m, every 20 minutes, 256 busy
  placements each), reading the lowest `MemAvailable` in its window:

  | Lowest in the burst window | Median | p10 | Worst | Below 0 | Below 6.2G | Below 10G |
  |---|---|---|---|---|---|---|
  | Warm, plain | 20.25G | 10.22G | 1.46G | 0 | 8 | 23 |
  | Warm, top-2 | 20.25G | 9.86G | 1.36G | 0 | 9 | 26 |
  | Cold, plain | 15.35G | 7.62G | 0.27G | 0 | 17 | 52 |
  | Cold, top-2 | 15.28G | 7.54G | 0.35G | 0 | 14 | 47 |

  Paired, top-2 is lower in 75 of 256 placements warm and 53 cold, by a median of 0.00G and at worst 7.82G warm and
  6.33G cold. That worst is the idle buffer the owner agreed to spend.
- **When the credit falls, starts wait; nothing running stops.** The reserve is the two largest jobs' headroom, not a
  pool any growth draws from: a job outside the top two that grows 1G raises `hosted` by 1G, and as the headroom left
  shrinks toward the floor, `hosted` rises toward `plain`. The replay's `OVER` column counts the looks where a credit's
  fall alone took the count past a budget it was within at the previous look: 3,032 summed over the warm alignments
  and 4,479 cold, against 146 and 1,388 for the plain count, whose falls are `MemoryMax`'s credit's alone. Nothing acts on that but waiting: `over_budget`
  reads `MemoryMax`'s count, and `hosted ≤ plain` at every state.
- **A restart mid-credit holds every start until the plain count fits.** Adopted runners never become eligible, so
  `Left.host` goes short by the credit that was spent, and nothing starts until enough adopted jobs end. On a heavy
  night that means waiting for the longest adopted jvm jobs. It is safe; it is not quick.
- **The host's other memory rises with CI.** The replay holds memory outside the slice as recorded, but it grows with
  what runs. The median of `total − available − slice` by runners running, 11-01 to 11-07:

  | Runners running | 0 | 4–7 | 8–11 | 12–15 | 16+ |
  |---|---|---|---|---|---|
  | 06:00–24:00 UTC | 8.1G | 11.3G | 15.4G | 15.3G | 16.7G |
  | 00:00–06:00 UTC | — (7 readings) | 14.0G | 15.8G | 17.0G | 16.3G |

  Seven of the eight largest 5-minute rises fall on 11-06 between 01:47 and 08:51 UTC: outside the slice, source
  unknown, during heavy CI. It may be the owner's agents building at night or per-container memory outside the slice
  (dockerd, containerd shims, sysbox). Whatever part a runner adds is not in `hosted`, and top-2 runs more at once, so
  the replay favours it by an amount it cannot measure. The sandbox run below measures it.
- **Limits of the replay.** Run times stay as recorded, the desktop does not react to a fuller slice, swap is not
  modelled, and neither are stalls or missing readings. The gates (stall, stale sample, missing host reading,
  `pooled_host`, `caps`, restart) are covered only by the generated engine sequences, which assert: `hosted ≤ plain`;
  `hosted` at least ineligible takes plus eligible use plus `min(ΣH, 6G)`; a start moves `hosted` by its take; `Drained`
  does not read the credit; no credit on a stale reading or while stalling; none after a restart.

### The release's proof

Under the sandbox lock, on a copy of the history, pools unlike the owner's, a 25G `MemAvailable` guard on every hog, and
2 s samples of `MemAvailable`, `SwapFree`, PSI `full`, the slice, oomd lines and `sock_throttled`:

1. **The credit admits.** Learn four distinct `ramp-N` keys (1G for 120 s, then 5G for 60 s; three runs each), so each
   reserves about 5.75G by `Job`. A 10G hog outside the slice holds the host limit. Run all four. Past 60 s the status
   shows a host credit of about `4 × 4.75 − 9.5 ≈ 9.5G`, and a queued 6G anchor starts on it. With
   `pooled_host = false` (second arm, same load) the anchor waits for the host.
2. **The accounting follows growth.** As the ramps grow, the status's `hosted` (`memory_in_use` less the host credit)
   matches `ineligible takes + committed + eligible use + min(ΣH, max(6G, H₁ + H₂))` from the runners' samples at each
   reading, and rises toward `plain`: with all four ramps grown 4G, ΣH is 3G and `hosted` equals `plain`. Starts wait;
   no running runner stops; the hog's 25G guard stays on. (The guard decides how low the host
   goes, so this measures the accounting, not the hazard; the replay is the hazard evidence.)
3. **What a runner costs outside the slice.** Start 8 idle runners with no jobs and record `total − available − slice`
   before and after. At 0.2G a runner or more, either count it in `committed` for the host or state it here as a known
   bias.
4. **Restart mid-credit.** `runnerd stop` while the ramps run, then start. The adopted runners count in full, the status
   shows no host credit, and nothing starts until the plain count fits.
5. **Stall.** Drive `full` avg10 over 5% with `MemAvailable` under twice the margin: the credit is 0 at the next plan.
6. **Wire and words.** A 0.0.29 `runnerctl status` reads the daemon while a host credit stands. The new host line and
   the held-with-room wording appear. The log has no `the host is short of memory` line while 30G or more is available.

Scenarios 1 to 4 pass before the release.
