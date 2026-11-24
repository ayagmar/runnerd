# 0172. `simulate` names the guard that held each wait

Status: **Accepted** (2026-11-24). Amends [0099](0099-the-budget-counts-jobs-that-do-not-peak-at-once.md),
[0130](0130-the-budgets-count-what-runs-uses-and-keep-room-for-its-largest-growths.md). Adds `admission::Guard` and
`Replay::guards`.

## Context

On the owner's host, `runnerctl insights` put most of the queue time of `jvm`, `jvm-heavy` and `node` on the host's
memory and the memory budget, while `runnerctl status` showed 25G of 62G available. Two causes were suspected for small
jobs:

1. **The 1G page cache room inflates small jobs.** Sizing gives a `light` job that peaks at 300M a need of 1.5G.
2. **`POOL_AFTER` keeps short jobs young for their whole run.** `light` and `jvm-small` jobs run about 30 s, under the
   60 s the growth count covers every young runner's growth for (`pooled::Young::Covered`).

`runnerd simulate --causal` could replay both, but it said only how long jobs waited for the slice and for the host,
not which rule held them, so neither could be read off a replay.

## Decision

- **A replay charges each wait to one guard** (`admission::Guard`, `Replay::guards`): at each plan, every job left
  waiting adds the time to the next plan to the guard that held it. The guards are a free slot, a barrier, the
  high-water mark, the CPU budget, `MemoryMax` and the host's memory, plus three rules that can hold a job its memory
  held: the host's larger margin, the growth count's room for every young runner, and that count's ceiling.
- **A rule gets the time only when relaxing it alone would have started the job at its turn.** The probe runs the same
  checks admission makes, in its order (`MemoryMax`, the high-water mark, the host, the CPU budget), against the count
  with only that rule relaxed, and the job must also pass any barrier standing then, as the plan would let it. The rest
  of the plan stays as it was. Where more than one rule alone would start it, the first of the young runners' room, the
  ceiling and the larger margin gets the time. Where none alone would, as when only two relaxed together would, the
  check that refused the job gets it: `MAX` or `HOST`. So `HOST` is the time the host's limit refused a job that no one
  of these rules held alone. It is not a measure of the plain margin, or of the room kept for the largest growths.
- **A job's time counts once it starts.** Each job keeps its own guard times until it starts, then adds them to the
  table, so the guards add up to the waits of the jobs that started (`Replay::waited`, `WAIT H`), both to the
  millisecond. A job never started counts in neither. `simulate --causal` prints the table, a row per count; each cell
  is rounded down to a tenth of a job-hour on its own, so the cells may add up to a little less than `WAIT H`.
- **The page cache room stays.** It sets caps, not reservations: measured admission reserves a job's predicted peak
  plus the margin, rounded up to 256M (ADR 0055). Over 7 days of the owner's history, 76% of 12,392 `light` runs
  reserved 512M and 93% at most 768M, for a mean peak of 323M; `jvm-small` was the same (72% at 512M, 95% at most
  768M of 1,218 runs). No light runner sample came more than 6 s after the last in 2 days, so none counted at its
  cap for a stale sample. The edge ADR 0055 measured is an absolute 400M to 500M of room for page cache, not a share
  of the peak, so a room that shrinks with the peak would bring back the socket throttling it fixed.
- **`POOL_AFTER` and `Young::Covered` stay.** Replayed over the owner's last 7 and 14 days to 2026-11-24, 46G
  `MemoryMax`, as the jobs really ran, with the daemon's growth count and builds that changed only the young rule:

  | count | 7 days: waits | 14 days: waits | 14 days: credit falls past a budget |
  | --- | ---: | ---: | ---: |
  | daemon (`growth`) | 770.2 job-hours | 963.3 | 709 |
  | small runners (take up to 1G) not young | 783.2 | 952.7 | 810 |
  | a 30 s young window | 807.4 | 1001.8 | 754 |
  | young runners ranked with the others | 802.9 | - | 755 (7 days) |

  Neither narrower rule cuts waits on both windows, and each makes the count fall past a budget more often. Ranking
  young runners with the others waits longer too: more starts let the brake engage for longer (53 hours against 37 over
  7 days), and while it holds the count is the plain one.

  The guard table for the daemon's count, over the 7 days to a later copy of the same history (about 775 job-hours
  of waits, every job started): the host's refusals 530.7, `MemoryMax` 103.9, barriers 47.6, the larger margin alone
  34.8, the young runners' room alone 29.6, slots 27.8, the ceiling alone 0.2; the CPU budget (the config's 1.5 times
  32 CPUs) and the high-water mark held none. Over 14 days: 973.4, of which the host 569.6 and the young runners' room
  alone 152.5. Taking runners of at most 1G out of that room moved the waits by +1.7% over 7 days and -1.1% over 14,
  so the room does not cost small jobs much either way.

## Consequences

- A change to admission can be judged by the guard it relaxes. The guard table reads a replay, not the daemon: the
  daemon still records its own causes per job (`api::WaitCause`, ADR 0154).
- The attribution is marginal: relaxing two rules at once can start jobs neither starts alone, and those stay with
  the check that refused them. A rule's figure is how much it alone held, not how much removing it gives back, since
  starts change what happens after. A replay with the rule changed says that.
- Most of the owner's waiting is the host's limit refusing jobs that no one of these rules held alone. What part of
  that the room kept for the largest growths, or the margin, accounts for needs its own replay; changing either is a
  safety decision about the host, not one about small jobs, and is left to its own change.
- Nothing changes in the daemon, the API or the history.
