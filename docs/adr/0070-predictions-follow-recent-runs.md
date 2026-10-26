# 0070. Predictions and memory sizing follow each job's recent runs

Status: **Proposed** (2026-10-26). Amends [0032](0032-sizing-advice-from-sampled-peaks.md),
[0033](0033-measured-admission.md), [0036](0036-young-jobs-predicted-from-their-own-runs.md),
[0039](0039-auto-sizing.md) and [0055](0055-admission-follows-the-hosts-memory.md).

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
  days' largest peak. jvm went 9G → 14G over two days, partly because a JVM's default heap is a quarter of its
  cgroup's limit, and stays there for 30 days after its jobs stopped needing it.

A job that got lighter keeps its old reservation for 7 days and its profile's cap for 30. Every window is a time,
so a job that runs ten times a day needs 70 light runs to shed one heavy run, and 300 to shed it from sizing.

A JVM's peak also says less than it seems. A JVM grows its heap toward its limit before it collects: `-Xmx` when
set, a quarter of the cgroup's limit by default. Its peak follows the room it had, not what it needs. The Android
gate peaked at 8.8G under 14G and passes a full cold run under an 8G cap, with no swap. Sizing reads a peak plus
the margin, or plus 1G of page cache room (0055), as the job's need, so a peak near the cap asks for a raise, the
raise gives the heap more room, and the next peak asks again. The owner set jvm to 8G × 4 slots by hand on
2026-10-26 to try it.

## Decision

A job's evidence is its last runs, counted, not its last days. One function in core reads it from the history
(`admission::Recent`, built from `sizing::History`); the predictor, `admission::simulate`, `sizing::advise` and auto
sizing all take it from there, so `runnerctl sizing`, `runnerd simulate` and admission say the same thing. A job is
keyed, as now, by repository, workflow, name and the profile it ran in (0058).

### The window

- **Counted runs.** A finished run, failed or lost ones included, whose memory was sampled without page cache; and
  a cancelled run that met its cap (`Ran::met_cap`, against the cap its container had). Other cancelled runs, and
  runs sampled with `active_file` (`Measure::WithActiveFile`), add no peak, as now.
- **A run that met its cap counts at its cap**, or its peak if more: the cap stopped it, so it needed at least that.
  A run that met an older, smaller cap counts at that cap, not at today's.
- **The window** is the job's counted runs from its `RECENT_RUNS`-th latest successful one on, in the order they
  finished; all of them with fewer. `RECENT_RUNS` is 20, or `[sizing] min_jobs` when that is more. Failed runs in it
  add their peaks but never push a successful run out, so a burst of failures cannot shrink the window to them.
  Within the history's 30 days, as now.
- 20 because under nearest rank a p95 of fewer than 20 values is their largest: 20 is the fewest at which the p95
  can leave one outlier out. A job that got lighter is predicted from its new runs once 19 of them finished.

Order: by `finished_at` (`queued_at` without one), ties in the store's row order, so the same rows give the same
window. Nothing is kept in memory between reads: the engine already rebuilds its predictor from the store every
minute, so a restart reads what the daemon would have.

### Predictions

Each of 0033's and 0036's rules reads the window in place of the job's 30 days: its `runs` is the window's count
(at most 20 by default), its p95 and largest the window's, its successful runs for `OWN_RUNS` the window's. A
profile's peaks, for `Basis::Profile` and `Basis::ProfileMax`, are its jobs' windows together, so a heavy job that got
lighter stops setting the size of a new one.

### A peak near the cap

A run met its cap when the kernel says so: its container's `memory.events` counted an `oom`, or the kernel killed a
process in it for memory (`oom_kill`, or Docker's `OOMKilled`). A run whose events were recorded and show neither
(`hit_cap: Some(false)`) did not meet its cap, however near it peaked: the kernel found the memory it asked for,
and a JVM in it collected instead of growing. Such a run says its job needs **at most** the cap it had, never more.
It does not start a hold, does not count at its cap in the window, and does not ask for a raise. `max`, which counts
every reclaim at the limit, page cache's included, is not read for this, as now (0037).

Only a run without recorded events (`hit_cap: None`, a container whose `memory.events` could not be read) keeps
the old fallback: a peak within 5% of its cap met it, since nothing else tells.

**The hold at the cap** (`Basis::Unpredictable`). A job's latest run that met its cap, cancelled or not, and one
sampled with `active_file` that was killed for memory, puts the job on its cap. The hold ends at the earlier of:

- `CLEAN_RUNS` (3) successful runs in a row after it that were each clear of their cap: did not meet it, as above,
  and peaked no more than they reserved when that was recorded. A peak near the cap with no `oom` is clear. A
  successful run that is not clear starts the count again. Failed runs and cancelled ones that did not meet their
  cap neither count nor start it again: they may have stopped before their heaviest step.
- `UNPREDICTABLE_FOR` (7 days) after it, as now. On the wire `until` stays that time and now reads "at the latest".

A prediction is never above the cap the job runs under now (0033), so runs that peaked higher under a larger cap
reserve the current one, not more.

**A run past what it reserved** (`Basis::Overran`): while a run of the job, cancelled or not, that peaked past what it
reserved is in its window (finished at or after the window's start), the job reserves the larger of the window's
largest peak and those runs' peaks, plus the margin. It ends when 20 later successful runs push that run out of the
window, or 7 days after it, whichever comes first.

This hold does not end after 3 clear runs, as the hold at the cap does. An overrun means the window's p95 missed one of its
peaks. Three lighter runs do not say the peak is gone, and after them the p95 would leave that run out again, so a
job heavy once in 20 runs would overrun once in every 20. Holding to the window's largest while the run is in the
window keeps that job reserving its heavy peak, and a job that really got lighter loses the hold in 20 runs instead
of 7 days.

### Sizing

A job's peaks (`api::JobSizing::peaks`) are its window; a profile's (`api::ProfileSizing::peaks`) its jobs' windows
together. `jobs`, `at_cap` and the spreads are counted over those runs, so the advice and the numbers it names
agree. Each run in a window says something different about the profile's `memory`, by how it ended:

| The run | It needs | `api::need` is the peak plus `margin` or `PAGE_CACHE_ROOM`, rounded |
|---|---|---|
| Met its cap X | more than X | `need(max(peak, X))`, above X |
| Ran clear of its cap X | at most X, and maybe its peak | `need(peak)`, but no more than X |

- **A raise** goes by the runs whose container had the profile's current `memory` and met it: the advice raises
  toward the largest of their needs. Nothing else raises `memory`. A run clear of the current cap does not, however
  near it peaked, and nor does a run at another cap: one that met a larger cap was too big for that cap, not shown
  too big for this one, and one that ran under a larger cap had other room. So a peak that grows with the cap never
  asks for more, and a hand-set cap stands until a run at it meets it. This drops 0055's "at least the largest peak
  plus 1G" for raises: a run that ran clear of its cap had the room it needed. Lowerings keep it.
- **A lowering** goes toward the largest need over every window, each run's by the table, so it never goes under a
  peak in a window, plus the margin or room, nor to or under a cap a run in a window met. Runs at a larger cap count
  too, at no more than that cap: a job heavy at 14G that has not run at 10G yet still holds 10G up.
- **A cap the profile outgrew.** A run that met a cap X smaller than the profile's current `memory` holds every
  lowering at `need(X)` or more for the history's 30 days, inside the window or not: the profile went past X because
  X was too small. A run that met the current cap or a larger one holds only through its window and the hold below:
  that is the Android gate's 14G run, and holding a lowering under it for 30 days is what this replaces.
- **The hold.** `Peaks` gains `held`: how many of the jobs are on a hold at the cap by the rule above, without its 7
  days (sizing reads 30 days of history; a job held past 7 days that has not run since is still one that may need
  its cap). No lowering is advised, and auto sizing lowers nothing, while `held` is above 0. That replaces 0032's
  "a set with a run at its cap is never lowered" and 0039's "no lowering while a run in the 30 days met its cap".
- Auto sizing's steps are unchanged: a lowering waits `interval` after the last change and moves at most `max_step`
  percent, a raise waits `raise_interval`. So jvm comes down toward its jobs' recent largest peak plus the room one
  step per interval, once the at-cap run left the Android gate's window and no job is held.

**Why it converges.**

- *Up.* A raise needs a run at the current cap that the kernel stopped at it. A JVM that fills whatever room it has
  without an `oom` never raises its profile, so there is no loop of peak → raise → bigger heap → peak.
- *Down.* A peak that shrinks with the cap can lead lowering down, one step at a time: each step waits `interval`,
  moves at most `max_step`, saves at least a tenth (`WORTH_LOWERING`), and waits for the runs at the larger cap to
  leave the windows, since they hold the need at their own peaks. It stops at the first cap where the heaviest job's
  peak plus the margin or room comes within a tenth of the cap without an `oom`: there the job collects instead of
  growing, which the 8G trial shows the Android gate does. If a step goes too far and a run meets the new cap X,
  the profile is raised toward `need(X)` after `raise_interval` and never lowered under `need(X)` for 30 days. So a
  profile is killed at a lowered cap at most once per cap per 30 days, and each such kill moves its floor up.
- *No swing faster than the intervals.* A raise waits `raise_interval` after the last change, a lowering `interval`,
  and no lowering comes while a job is held: 3 clear runs at the new cap after a kill.

Peaks do not say whether a job at a lower cap runs slower because it collects more often. That costs run time, not
kills, and is accepted, as 0068 accepted it for CPUs: the history cannot tell it from other changes to the job.

### The wire

No schema change and no stored-data migration: every input is a column the history already has.

- `api::Peaks` gains `held`, skipped when 0. A 0.0.17 reader ignores it.
- `Basis` is unchanged. `Unpredictable` and `Overran` keep `until`, now the latest the hold lasts. A 0.0.17 reader
  shows the same line as before.
- Core words the new facts: `Admission::line` says a `Job` basis is the p95 or largest of "its last N runs", and a
  held profile's advice line says how many of its jobs met their cap and how few runs since were clear of it. Clients
  render these, with `crates/fixtures` cases for both; wire-compat samples gain a `Peaks` with `held`.

## Consequences

- The Android quality gate reserves its p95 again 3 clear runs after its at-cap run, and its new peaks once 19 runs
  at them finished: a day or two at its rate, not 7 days. web-verify's reservation follows its 5G peaks in the same
  number of runs. Left at 14G, jvm would step down once the at-cap run left the window, a step per `interval`.
- Accepted: a job heavy less often than once in 20 successful runs is predicted, and sized, for its light runs. Each
  heavy run then goes past what it reserved, bounded by its cap, or meets a cap auto sizing lowered and is killed or
  held at it. The job then reserves and is sized for that run until 20 more push it out. Under the old rule that
  cost came once a week for admission and once a month for sizing. A job like that should get its cap from
  `min_memory`.
- Accepted: after a hold at the cap ends, the window's p95 can leave the at-cap run out. If that peak comes back
  within the window, it is an overrun, and the job reserves the window's largest from then on. A job that meets its
  cap twice in its window reserves the cap at p95 anyway.
- A job whose runs keep failing ends a hold only after 7 days, as now.
- A job that runs a few times a month has fewer than 20 runs in 30 days, so its window is its whole history, as now.
- `held` ignores the 7 days, so sizing holds a profile longer than admission holds its job when the job stops
  running after meeting its cap. It holds at most as long as now.
- The owner's 8G jvm stays: runs at 14G peaked higher, but they ran at another cap and met none at 8G, so nothing
  raises it, and while they are in the windows they hold any lowering under 10.25G off. Without them, it goes lower
  only once its runs at 8G peak at about 6.2G or less: their need, with the 1G room, must save a tenth of 8G.
- A non-JVM job whose cap is lowered by hand under its real need is killed once at the new cap before sizing raises
  it: runs at the larger cap no longer raise.
- A run that ran clear of its cap with less than 1G of room may still see its downloads throttled (0055). Sizing no
  longer raises for that; the `throttled` insight still names the repository.

## Tests

Each through `Predictor`, `simulate`, `sizing::advise` or `auto::changes`, named in domain terms, with the bug it
catches.

Generated sequences: a seeded generator in the test module (no new dependency) draws runs of a few jobs: light,
heavy, at the cap, killed for memory, failed, cancelled, cancelled at the cap, past what they reserved, sampled with
`active_file`, under a cap that changes mid-way. Over each sequence and every prefix of it:

1. **Rebuilding from the history decides as learning run by run did**: the prediction from a predictor built from
   the stored rows of a prefix equals the one `simulate`'s incremental predictor holds at that point, and a
   restarted engine reserves what one that did not restart reserves. Catches state that lives only in memory, and a
   window that depends on the order runs were learned in rather than finished.
2. **Never above the cap; never under a peak in the window that went past what it reserved.** Catches an `Overran`
   hold that ends early or reads the wrong window.
3. **On the cap while fewer than 3 clear successful runs followed its latest cap event, within 7 days.** Catches a
   counter that a failed or cancelled run advances, that a non-clear success does not reset, or that judges a run
   against today's cap instead of its own.
4. **Advice and auto sizing never lower under a peak, or a met cap, in a job's window, nor while `held` > 0**, and
   no two changes of a profile come closer than `raise_interval`, nor a lowering closer than `interval` to the last
   change. Catches a ping-pong after an OOM at a lowered cap.

5. **No change without a cap event raises `memory`, and no lowering goes under the need of a run in a window or
   `need(X)` of a cap X the profile outgrew in 30 days.** Over generated days of a job whose peak is a share of its
   cap (a JVM) and that is killed only under a fixed working set: `memory` never rises without a run at the current
   cap that met it, ends at a fixed point, and is killed at most once per cap. Catches a raise from a peak that
   follows the room, and a lowering that keeps chasing it.

Cases:

6. A job of 352 heavy runs and one at its cap that gets lighter: on its cap until 3 clear runs, its p95 from the
   fourth, its light peak + margin from the 19th light run (the trellis case). Catches a window by time.
7. Runs alternating light and at the cap stay on the cap however long the sequence. Catches counting clear runs that
   are not in a row.
8. A run that met a 9G cap followed by runs clear of a 14G cap: counted at 9G, and the hold ends after 3. Catches
   comparing old runs with the new cap.
9. A job with only cancelled runs reserves its cap; a never-seen job its profile's largest windowed peak; a run
   sampled with `active_file` adds no peak, and its kill for memory still holds the job.
10. jvm at 14G whose jobs now peak at 8.8G steps down by `max_step` per `interval` toward 10.25G and stops there; an
   OOM at a lowered cap raises it after `raise_interval` and keeps it above that cap while the run is in the window.
11. `runnerd simulate` on the trellis-like history: the measured replay's reservations follow the light runs as the
    predictor's do (shared rule), and `runnerctl sizing`'s advice is the change auto sizing writes.
12. A run that peaked within 5% of its cap with events recorded and no `oom` starts no hold and raises nothing; with no
    events recorded it does both, as now. Catches a near-cap peak read as a need.
13. jvm set by hand to 8G, with the Android gate's 14G runs in its window and runs at 8G peaking at 7.6G without an
    `oom`: no raise, no lowering. A run at 8G that meets it raises toward `need(8G)`. Catches undoing the owner's
    trial from runs at another cap.

Fixtures in `crates/fixtures`: a held profile and a job predicted from its last 20 runs, for each client's parity
test. Wire compat: a 0.0.17 reader reads `Peaks` with `held`.
