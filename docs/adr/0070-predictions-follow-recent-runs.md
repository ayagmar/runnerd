# 0070. Predictions and memory sizing follow each job's recent runs

Status: **Proposed** (2026-10-26). Amends [0032](0032-sizing-advice-from-sampled-peaks.md),
[0033](0033-measured-admission.md), [0036](0036-young-jobs-predicted-from-their-own-runs.md),
[0037](0037-memory-without-page-cache.md), [0039](0039-auto-sizing.md) and
[0055](0055-admission-follows-the-hosts-memory.md).

## Context

On the owner's host, 2026-10-26, jobs waited 10–30 minutes on a 46G slice that was mostly reserved for memory no
job used.

- trellis "Android quality gate" peaked at 10.5–11.3G until a workflow change gave Gradle and Kotlin explicit
  heaps; since then it peaks at 8.5–8.8G. Its 352 runs in the 30-day history have a p95 of 11.48G, and one run met
  the 14G cap, so admission reserves the whole cap (`Basis::Unpredictable`) for 7 days.
- glowly "web-verify" peaked at up to 8.34G until its vitest and turbo workers were bounded; it now peaks near 5G
  and still reserves 9.75G.
- A runner takes whichever of its repository's queued jobs GitHub hands it, so it starts only when the largest
  queued prediction fits (0033). A 1.7G trellis job waits for 14G free, a 1.5G image build for 9.75G.
- Auto sizing never lowers `memory` while a run of the profile in the 30 days met its cap, and advises the 30
  days' largest peak. jvm went 9G → 14G over two days and stays there for 30 days after its jobs stopped needing
  it.

A job that got lighter keeps its old reservation for 7 days and its profile's cap for 30. Every window is a time,
so a job that runs ten times a day needs 70 light runs to shed one heavy run, and 300 to shed it from sizing.

A JVM's peak also says less than it seems. A JVM grows its heap toward its limit before it collects: `-Xmx` when
set, a quarter of the cgroup's limit by default. Its peak follows the room it had, not what it needs. The Android
gate peaked at 8.8G under 14G and passes a full cold run under an 8G cap, with no swap. Sizing reads a peak plus
the margin, or plus 1G of page cache room (0055), as the job's need, so a peak near the cap asks for a raise, the
raise gives the heap more room, and the next peak asks again. The owner set jvm to 8G × 4 slots by hand on
2026-10-26 to try it.

The two uses of a peak carry different risks. A reservation too small lets runs overlap past the slice's budget,
which the samples and the caps bound. A cap too small kills a job. So admission may follow a job's last runs
closely, and the cap must not.

## Decision

A job's evidence is its last runs, counted, not its last days. One module in core reads it from the history
(`admission::Recent`, from `sizing::History` rows), with one rule for what each row says; the predictor,
`admission::simulate`, `sizing::advise` and auto sizing all take it from there. A job is keyed, as now, by
repository, workflow, name and the profile it ran in (0058).

### What a run says about its cap

The kernel's `oom` counter tells when a container's limit stopped an allocation. It is not a signal of memory
failures in general: a JVM or Node process whose heap is sized from the cgroup's limit fails on its own heap
(`OutOfMemoryError`, "JavaScript heap out of memory") without the kernel ever reaching the limit. That case has its
own rule under [Sizing](#sizing).

- **Met its cap**: the container's own limit failed reclaim, `oom` in `memory.events.local` above 0. The
  hierarchical `memory.events`, read until now, also counts a limit any descendant cgroup hit: under sysbox a job's
  own containers (`services:` with `--memory`, a compose `mem_limit`, a Testcontainers limit) are descendants, so a
  512M test container killed at its own limit made a 3G run "meet" its 14G cap. Each runner's row gains the local
  counter (schema 14, `runners.local_ooms`, the largest value sampled, as `ooms` is).
- **Killed for memory**: a process in the container was killed (hierarchical `oom_kill`, or Docker's `OOMKilled`)
  and the run did not meet its cap. When the counters show only a descendant's limit (hierarchical `oom` above
  the local one), the kill was inside a container the job set its own limit for, and it says nothing of the job's
  memory: it is ignored. Otherwise the slice or the host ran out, which admission must learn.
  `memory.events.local`'s own `oom_kill` is not read: the kernel counts a kill at the victim's cgroup, which under
  sysbox can be a child of the container even when the container's limit was hit.
- **Clear of its cap**: neither. A run whose events were recorded and show no local `oom` is clear however near its
  cap it peaked. That is reliable as "no 2 s sample saw this container's limit fail reclaim"; an `oom` without a kill
  in a run's last interval is missed, which is rare. A kill in the last interval is still caught by Docker's flag:
  when no sample saw an `oom` at any level and the last one came within 5% of the cap, the run met it.
- **Rows without the local counter**, from before schema 14, and runs whose events could not be read: today's
  reading holds for holds and lowering floors (the hierarchical `oom`, or without events a peak within 5% of the cap
  met it), but such a run never raises a cap or reverts a lowering.

### The windows

- **Counted runs.** A finished run, failed or lost ones included, whose memory was sampled without page cache; and
  a cancelled run that met its cap. Other cancelled runs, and runs sampled with `active_file`
  (`Measure::WithActiveFile`), add no peak, as now.
- **A run that met its cap counts at its cap**, or its peak if more: the cap stopped it, so it needed at least that.
  A run that met an older, smaller cap counts at that cap, not at today's. A run killed for memory counts at its peak.
- **A window** is the job's counted runs from its n-th latest successful one on, in the order they finished; all of
  them with fewer. Failed runs in it add their peaks but never push a successful run out, so a burst of failures
  cannot shrink it to them. Within the history's 30 days.
- **Admission's window** is `RECENT_RUNS` = 20 successful runs, or `[sizing] min_jobs` when that is more: under
  nearest rank a p95 of fewer than 20 values is their largest, so 20 is the fewest at which it can leave one outlier
  out. A job that got lighter reserves its new peaks once 19 of them finished.
- **Sizing's window** is three times as long, `SIZING_RUNS` = 60 successful runs by default. A cold-cache run, after a
  dependency bump or a lockfile change, is heavy once in 20 to 50 runs; a cap lowered on 20 runs would sit under it
  for a third of the time. 60 runs is about 6 days of the Android gate, against 30 now.

Rows are read in `finished_at` order (`queued_at` without one), ties in the store's row order. `Recent` takes the
rows in that order whoever feeds it, and does the per-row handling the engine does inline now: the cap of record,
`measure`, and cancelled runs. Each job keeps its window as runs in that order and drops the ones before the
window's start as a new successful run arrives; its sorted peaks, and its profile's, are rebuilt from the window
when it changes, at most 60 values a job. Nothing else is kept between reads, so a restart reads what the daemon
would have.

### Predictions

Each of 0033's and 0036's rules reads admission's window in place of the job's 30 days: its `runs` is the
window's count, its p95 and largest the window's, its successful runs for `OWN_RUNS` the window's. A profile's peaks,
for `Basis::Profile` and `Basis::ProfileMax`, are its jobs' windows together, so a heavy job that got lighter stops
setting the size of a new one. A prediction is never above the cap the job runs under now (0033), so runs that
peaked higher under a larger cap reserve the current one, not more.

**The hold at the cap** (`Basis::Unpredictable`). A job's latest run that met its cap or was killed for memory,
cancelled or not, puts the job on its cap. The hold ends at the earlier of:

- `CLEAN_RUNS` (3) successful runs in a row after it, each clear of its cap and, when a reservation was recorded,
  peaking no more than it. A run with no recorded reservation, under caps admission or adopted, is clear when it is
  clear of its cap. A successful run that is not clear starts the count again. Failed runs and cancelled ones that
  are clear neither count nor start it again: they may have stopped before their heaviest step.
- `UNPREDICTABLE_FOR` (7 days) after it, as now. On the wire `until` stays that time and now means "at the latest".

**A run past what it reserved** (`Basis::Overran`): while a run of the job, cancelled or not, that peaked past what it
reserved finished at or after its window's start, the job reserves the larger of the window's largest peak and those
runs' peaks, plus the margin. It ends when 20 later successful runs push that run out of the window, or 7 days
after it, whichever comes first.

This hold does not end after 3 clear runs, as the hold at the cap does. An overrun means the window's p95 missed one
of its peaks. Three lighter runs do not say the peak is gone, and after them the p95 would leave that run out again,
so a job heavy once in 20 runs would overrun once in every 20. Holding to the window's largest while the run is in
the window keeps that job reserving its heavy peak, and a job that really got lighter loses the hold in 20 runs.

A job's run failing on its own heap holds nothing in admission: it used less of the slice, not more.

### Sizing

A job's peaks (`api::JobSizing::peaks`) are its sizing window; a profile's (`api::ProfileSizing::peaks`) its jobs'
sizing windows together. `jobs`, `at_cap` and the spreads are counted over those runs, so the numbers the advice
names are the ones it went by. The windows always come from the 30 days to now: `GET /v1/sizing?since=`, and
`runnerctl sizing --since`, narrow only the waits, the busy share and the starts, so the advice stays what auto
sizing writes.

Core computes two needs per profile, from different runs, and the advice goes by them:

- **`raise_need`**: over the runs that met the profile's current `memory` X and finished since the profile last
  took X, by its `profile.changed` events, as `cpus` reads runs since it took its `cpus` (0068): the largest of
  `need(max(peak, X))` and, for each such run's job, that job's `lower_floor` (below) from its windowed runs at
  larger caps. A run's last 2 s sample before a kill rarely shows what it needed, so `need(X)` alone would climb
  back one kill per step; a job that ran clear at 14G and is killed at a hand-set 8G is raised toward what its 14G
  runs needed at once. Nothing else raises `memory`: not a run clear of X however near it peaked, not a run that met
  another cap, not a run at X from an earlier time at X. A peak that grows with the cap never asks for more, and a
  hand-set cap stands until a run at it meets it. This drops 0055's "at least the largest peak plus 1G" for raises:
  a run clear of its cap had the room it needed. Lowerings keep it.
- **`lower_floor`**: the largest of each windowed run's need, by how it ended:

  | The run | It needs | Its need (`api::need`: the peak plus `margin` or 1G, rounded) |
  |---|---|---|
  | Met its cap X | more than X | `need(max(peak, X))`, above X |
  | Clear of its cap X, or killed for memory under it | at most X, and maybe its peak | `need(peak)`, at most X |

  and the floor of any revert in the 30 days (below). Runs at a larger cap count, at no more than that cap: a job
  heavy at 14G that has not run at 10G yet still holds 10G up.

`Advice` is `Raise` toward `raise_need` when it is above the cap, `Lower` toward `lower_floor` when that saves a
tenth and no job is on its cap, and `Keep` otherwise. A `Keep` whose `lower_floor` is above the cap says why
without asking for more, as core words it: "keep 8G: its recent runs need 10G, but none is known to have run out of
memory at 8G since it was set". `SliceTooSmall` goes by `raise_need`. Profiles under measured admission add slots on the windows' p95 peaks (`measured_slots`); that is
intended, and adds slots sooner after a job got lighter.

**On its cap.** `Peaks` gains `on_cap`: how many of the jobs are on a hold at the cap by the rule above, without its
7 days (a job held past 7 days that has not run since may still need its cap). Not `held`, which
`api::AutoSizing` already uses for profiles a recent change holds. No lowering is advised while `on_cap` is above 0.
That replaces 0032's "a set with a run at its cap is never lowered" and 0039's "no lowering while a run in the 30
days met its cap".

**A lowering that went too far is reverted.** Auto sizing's latest change of `memory` (changes of only `slots` or
`cpus` since do not count) set the profile's current `memory` X and lowered
it from F, by its recorded change (`SizingChange`), and the profile has run X since, by its `profile.changed`
events. The lowering went too far when, since then, either:

- a run at X met it; or
- a job with a successful run at F in the history (before the lowering; after the revert, its runs at F since keep
  the floor while the failures are in the history) failed `FAILED_RUNS` (3) times in a row at X, with no
  success between: the case a heap sized from the cgroup's limit fails without a kernel event. Only conclusions
  `failure` and `timed_out` count (a heap thrashing in GC times out); `lost` and `cancelled` neither count nor break
  the row. At most one failure counts per workflow run (`run_id`), so a re-run of failed jobs adds nothing, and the
  three must come from at least two branches, so one broken pull request pushed three times does not revert. Red CI
  across branches can still look the same; reverting it costs memory, not jobs.

The next look then writes `memory` F back, and under caps admission the slots it had before, without waiting
for `raise_interval`: it takes back auto sizing's own change, as an undo would. Under caps it waits, as a raise does,
while more runners run than F's slots hold. When F is outside the profile's bounds now (the owner lowered
`max_memory` under it), the profile is sized as any other, a raise going as far as the bounds allow. For the 30 days
after, no lowering goes under F. That floor is read from
the recorded changes and the runs, so a restart keeps it. A job that met X is on its cap until 3 clear runs;
the floor holds for one that failed at it.

A cap the owner set by hand is not reverted: a run that meets it raises it straight to `raise_need` after
`raise_interval`, unbound by `max_step`. A kill is direct evidence, and `raise_need` reads only runs that met the
current cap, so a stepped raise would stop at its first step until another run is killed at it.

The revert is advice like any other: `sizing::advise` takes the recorded changes beside the history, and words a
pending revert as `Raise` to F with the revert's reason, and its floor as part of `lower_floor`, so `runnerctl
sizing` shows during and after a revert what auto sizing writes.

Auto sizing's steps are otherwise unchanged: a lowering waits `interval` after the last change and moves at most
`max_step` percent, a raise waits `raise_interval` and goes straight to the advice.

**Why it converges.**

- *Up.* A raise needs a run at the current cap, since the profile took it, that the cap stopped. A JVM that fills
  whatever room it has without an `oom` never raises its profile, and a limit a job's own container hit never does.
- *Down.* A peak that shrinks with the cap can lead lowering down, one step at a time: each waits `interval`, moves
  at most `max_step`, saves a tenth (`WORTH_LOWERING`), and waits until the runs at the larger cap left the sizing
  windows, about 60 runs of the heaviest job, since they hold `lower_floor` at their own peaks. It stops at the first
  cap where the heaviest job's peak plus the margin or room comes within a tenth of it, which the 8G trial suggests
  for the Android gate: there the JVM collects instead of growing. A step that goes too far is reverted to the cap
  it came from on the first run that meets it, or the third failure in a row, and the profile stays at or above that
  cap for 30 days.
- *What a lowering costs.* At most the runs of the job that run at X before the revert is written: the run that met
  X, the copies of it already running beside it, and the 3 failures of the heap case. Then 30 days at F. A revert to
  `need(X)` instead could be met again at each step on the way back up, since a run's last 2 s sample before a kill
  rarely shows what it needed.
- *No swing faster than the intervals.* A raise waits `raise_interval` after the last change, a lowering `interval`
  and a revert's 30 days; no lowering comes while a job is on its cap.

Peaks do not say whether a job at a lower cap runs slower because it collects more often. That costs run time, not
kills, and is accepted, as 0068 accepted it for CPUs: the history cannot tell it from other changes to the job.

### The wire and the store

- Schema 14 adds `runners.local_ooms`. A 0.0.17 daemon refuses the newer database, as with every schema change;
  `runnerd simulate` and `doctor` read older ones, as `sizing_of` does.
- `api::Peaks` gains `on_cap`, `#[serde(default)]` and skipped when 0: a 0.0.17 reader ignores it, and a new client
  reads a 0.0.17 daemon's `Peaks` as none on their cap.
- `Basis` is unchanged. `Unpredictable` and `Overran` keep `until`, now the latest the hold lasts. A 0.0.17 reader
  shows the same line as before.
- Core words the new facts: `Admission::line` says a `Job` basis is the p95 or largest of "its last N runs"; a
  profile's advice line says how many of its jobs are on their cap, and a `Keep` above the cap says which runs the
  floor came from and that none met the current cap; a revert's reason names the run that met X or the job that
  failed at it. Clients render these lines as core words them; wire-compat samples gain a `Peaks` with `on_cap`
  and a `ProfileSizing` with `revert`.

## Consequences

- The Android quality gate reserves its p95 again 3 clear runs after its at-cap run, and its new peaks once 19 runs
  at them finished: a day or two at its rate, not 7 days. web-verify's reservation follows its 5G peaks in the same
  number of runs.
- The owner's 8G jvm stays. Runs at 14G peaked higher, but they ran at another cap and none met 8G, so nothing
  raises it. While they are in the sizing windows, about 6 days, they hold `lower_floor` at 10.25G, above 8G, so
  nothing lowers it. After that it goes lower only once its runs at 8G peak at about 6.2G or less: their need, with
  the 1G room, must save a tenth of 8G.
- Accepted: a job heavy less often than once in 60 successful runs is sized for its light runs. Its next heavy run
  meets the lowered cap: that run, and copies of it running then, are killed or fail, the profile goes back to the
  cap the lowering came from and stays at or above it for 30 days. Under the old rule a job like that was never
  killed and its profile never lowered. A job like that can keep its cap with `min_memory`.
- Accepted: admission's job heavy less often than once in 20 runs reserves its light runs; its heavy run goes past
  its reservation, bounded by its cap, and the job then reserves it for 20 runs. Under the old rule that cost came
  once a week.
- Accepted: after a hold at the cap ends, admission's p95 can leave the at-cap run out. If that peak comes back
  within the window, it is an overrun, and the job reserves the window's largest from then on. A job that meets its
  cap twice in its window reserves the cap at p95 anyway.
- A hand-lowered cap under a non-JVM job's real need is met once, then raised at once to what that job's runs at a
  larger cap needed. A job with no runs at a larger cap may be met once per raise, as each raise goes
  to `need(X)`. Only auto sizing's own lowerings are reverted.
- A run clear of its cap with less than 1G of room may see its downloads throttled (0055). Sizing no longer raises
  for that; the `throttled` insight still names the repository.
- A kill only a descendant's limit explains no longer holds a job in admission, and no longer holds its profile's
  memory.
- A job whose runs keep failing ends a hold only after 7 days in admission, as now.
- A job that runs a few times a month has fewer runs than a window in 30 days, so its windows are its whole history,
  as now.
- `on_cap` ignores the 7 days, so sizing holds a profile longer than admission holds its job when the job stops
  running after meeting its cap. It holds at most as long as now.
- `admission::simulate` replays a different past than happened: other starts, other reservations, other overlaps.
  It agrees with the daemon on the rule and on how each row is read, not on the outcome.

## Tests

Each through `Recent`, `Predictor`, `simulate`, `sizing::advise` or `auto::changes`, named in domain terms, with the
bug it catches.

Generated sequences: a seeded generator in the test module (no new dependency) draws runs of a few jobs: light,
heavy, cold-cache heavy once in 30, at the cap, killed for memory, killed at a descendant's limit, failed on its own
heap, cancelled, cancelled at the cap, past what they reserved, sampled with `active_file`, without the local counter,
under a cap that changes mid-way by hand and by auto sizing. Over each sequence:

1. **Rows learned one at a time in finished order give the predictor built from those rows at once**, and a
   restarted engine reserves what one that did not restart reserves. Catches state that lives only in memory, and a
   window that depends on the order rows were fed in.
2. **Never above the cap; never under a peak in admission's window that went past what it reserved.** Catches an
   `Overran` hold that ends early or reads the wrong window.
3. **On the cap while fewer than 3 clear successful runs followed its latest cap event, within 7 days.** Catches a
   counter that a failed or cancelled run advances, that a non-clear success does not reset, that reads a missing
   reservation as not clear, or that judges a run against today's cap instead of its own.
4. **Auto sizing never lowers under `lower_floor` or a revert's floor, nor while `on_cap` is above 0; never raises
   without a run that met the current cap since the profile took it; and no two changes of a profile come closer
   than `raise_interval`, or a lowering closer than `interval` to the last change, except a revert. A broken pull
   request pushed three times, or re-run twice, right after a lowering reverts nothing, nor do lost runs.** Catches
   a raise from old runs or from a peak that follows the room, a ping-pong, and a revert on ordinary red CI.
5. **Kills per lowering.** Over generated days of a JVM-like job, whose peak is a share of its cap and which fails on
   its heap under a fixed working set, and of a cold-cache job: each auto-sizing lowering is followed by at most one
   burst of kills or `FAILED_RUNS` failures at the lowered cap, and the profile is back at the cap it came from at
   the next look. `memory` ends at a fixed point. Catches a revert that climbs back a step at a time, and a lowering
   that keeps chasing a shrinking peak.

Cases:

6. A job of 352 heavy runs and one at its cap that gets lighter: on its cap until 3 clear runs, its p95 from the
   fourth, its light peak + margin from the 19th light run (the trellis case). Catches a window by time.
7. Runs alternating light and at the cap stay on the cap however long the sequence. Catches counting clear runs that
   are not in a row.
8. A run that met a 9G cap followed by runs clear of a 14G cap: counted at 9G, and the hold ends after 3. Catches
   comparing old runs with the new cap.
9. A job with only cancelled runs reserves its cap; a never-seen job its profile's largest windowed peak; a run
   sampled with `active_file` adds no peak, and its kill for memory still holds the job.
10. jvm at 14G whose jobs now peak at 8.8G steps down by `max_step` per `interval` toward 10.25G once the at-cap run
    left the sizing window, and stops there. A run then meets the lowered cap: the next look writes the cap it came
    from, and no lowering goes under it for 30 days, across a restart. The same with 3 failures in a row at the
    lowered cap and no kernel event. Catches a revert that waits `raise_interval`, goes to `need(X)`, or forgets its
    floor on restart.
11. A cold-cache job heavy once in 30 runs keeps its cap: its heavy run stays in the 60-run window. Catches sizing on
    admission's window.
12. A run that peaked within 5% of its cap with no local `oom` starts no hold and raises nothing; with no events
    recorded it holds and floors, as now, and raises nothing. A 3G run whose test container was killed at its own
    512M limit raises nothing and holds nothing. Catches a near-cap peak or a descendant's limit read as the cap.
13. jvm set by hand to 8G, with the Android gate's 14G runs in its windows and runs at 8G peaking at 7.6G without an
    `oom`: `Keep` with core's reason, no raise, no lowering, and no revert. A run at 8G that meets it raises toward
    `need(8G)`. jvm set by hand to 9G, where runs met 9G a week earlier at the same value: no raise until a run since
    the change meets it. Catches undoing the owner's trial from runs at another cap or another time.
14. `runnerctl sizing --since 2d` gives the advice auto sizing writes, and so does `runnerctl sizing` during and
    after a revert. A job killed at a hand-set 8G with runs clear at 14G is raised toward their need, not 9.25G.
    Catches windows read from `since`, a revert outside the advice, and a raise that climbs one kill per step.
15. `simulate` reads each row as the engine does: with room for every job, the reservation of each start equals what
    the predictor built from the rows finished before it gives. Catches simulate learning a run under today's cap or
    its sampled-with-page-cache peak.

The local counter's meaning is the kernel's: `memory.events.local` counts events at the cgroup's own limit, not a
descendant's. `observe`'s tests read it beside `memory.events`, and the store's tests read it back per runner and as
`None` from a schema 13 history. A Docker test with sysbox that kills a child container at its own limit is left for
a machine that runs them.

Clients render core's lines (`advice_line`, `Admission::line`) and its `at_cap` count as they are and compute none of
these facts, so core's tests assert the lines: a profile with jobs on their cap, a `Keep` above the cap with none
met, and a revert's reason. Wire compat: a 0.0.17 reader reads `Peaks` with
`on_cap` and a `ProfileSizing` with `revert`, and the new reader reads 0.0.17's without them.
