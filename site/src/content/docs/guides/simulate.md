---
title: What would another config have done?
description: runnerd simulate replays your job history through admission policies, slot counts and CPU budgets, and changes nothing.
---

`runnerd simulate` replays the last 30 days through each policy, every job queued when it was and holding its runner as long as it did, from the runner's start. It changes nothing. This history on 8 CPUs, node's slots raised to 8:

```text
$ runnerd simulate --memory-max 33G --slots node=8 --cpus 8 --cpu-overcommit 1 --cpu-overcommit 1.5
POLICY    SLOTS                   WAIT P50  WAIT P95  WAIT MAX      MEAN AT ONCE    PEAKS   UNDER  HELD    CPU CPU HELD
caps      jvm 2, node 6                 0s     4m19s     7m38s       57s       5   18.95G       0     0    6.2        -
measured  jvm 3, node 8                 0s     1m55s     3m20s       18s       8   24.35G       4     0      7        -
cpu ×1    jvm 3, node 8                 0s     2m30s     4m21s       32s       8   24.16G       4     0      7      281
cpu ×1.5  jvm 3, node 8                 0s     2m08s     4m21s       21s       8   24.35G       4     0      7        8
```

- `PEAKS`: every running job's peak added up at the busiest moment. `UNDER`: jobs that peaked above what admission reserved. `HELD`: starts the high-water mark held back.
- `cpu ×N`: a CPU budget of N times the slice's CPUs. `CPU`: the most the jobs' average CPUs added up to. `CPU HELD`: starts the budget held back.

Options: `--memory-max`, `--slots PROFILE=N`, `--since 7d` (counted back from the history's last job, so a backup replays its own last days), `--cpus`, and `--cpu-overcommit`, repeated to compare factors.

`--priority` adds the config's policy and slots replayed three ways: with no barrier for [a starved job](../../concepts/growth-and-priority/#a-starved-job-goes-first), with one nothing starts behind, and as the daemon runs. It shows each pool's waits, how long whole workflow runs took from first arrival to last end, the budget's busy share, and how many jobs started behind a barrier and overran. Every row above already goes as the daemon runs.

These replays know the history from before the window, as the daemon would. A job that queued after another of its run ended arrives as long after that job's replayed end, so a job held back also holds back what follows it. The history has no `needs`, so this is read from the times.

`runnerd simulate --causal` replays the config's policy with each job using what its own samples say, shifted to its replayed start, for the plain count and credits of 4G, 6G and 8G. A job counts as known when the daemon could name it, and the host's recorded readings bound what starts. Besides waits and run times it shows what each count risks: the slice past 90% and past `MemoryMax`, what runners used past the count that let them start, the host under 20G and 6.2G available, the time at its larger margin, and how often the credit fell. The first row, `RECORDED`, is what the same jobs did as they ran, and `VS REC` reads each row's waits against it: the replay's own error, so compare rows with each other, not with what happened. [runnerd simulate](../../reference/runnerd-simulate/) lists its options, as `--help` prints them.

Jobs still queued have no run time, so the waits leave them out and the report only counts them: the jobs that waited longest may be the ones it cannot show. Without `--causal` the replay has no host readings. It never has warm runners or restarts, and it gives each job the runner started for it, where GitHub may give that runner any job of its pool. With no host limit there is less contention than the daemon sees, so try a smaller `--memory-max` too. Each window starts from an empty slice, so moving its start by minutes can change the figures a lot.
