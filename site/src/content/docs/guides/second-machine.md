---
title: How do I add a second machine?
description: Run runnerd on a laptop beside the desktop, on the same labels, so the laptop takes only the jobs the desktop could not.
---

Each machine runs its own runnerd with its own config; nothing connects them but GitHub. Give the machine that should take the jobs first the default role, and the other one `role = "overflow"`. On the laptop, `init` writes that for you:

```sh
runnerd init --overflow halyard/ledger halyard/storefront
```

It writes the starter config `init` writes on any machine, sized for the laptop, with three changes: `role = "overflow"` under `[host]`, `warm = 0` on each repository, and `[sizing] auto = false`. `runnerd check-config` has no warning for it. To set it by hand instead:

```toml
[host]
name = "laptop"
role = "overflow"
overflow_after = "45s"   # the default
```

The laptop takes a job only when every label in its `runs-on` is in the repository's labels, so give each repository the labels it has on the desktop. `init` writes `self-hosted`, `linux` and the architecture, as on the desktop; change them if you changed the desktop's.

The overflow host starts a runner for a job only once GitHub queued it 45 s ago and it listed the job twice, the second time after a fresh listing of the repository's runners. If that listing shows a runner of the desktop that just came online idle with labels that fit the job, the laptop waits one more listing for GitHub to give the job to it. A job the desktop takes in time is gone from the queue before then, and the laptop never boots a runner for it. While the desktop is full, asleep or off, its jobs run on the laptop about 45 s late. Until then the job says *Left to the primary machine for 30s more* ([Why is my job waiting?](../waiting/)).

An overflow host keeps no warm runners and starts no bursts, whatever `warm` says, and stops an idle runner after 2 minutes instead of 5: GitHub gives a job to any idle runner that is online, and one left on the laptop would take the next job before the desktop starts one. `runnerd check-config` warns about each pool whose `warm` it ignores, and about `[sizing] auto`, which there would size the profiles from the jobs the desktop could not take.

- Run one primary per set of labels. Two primaries on the same labels each start a runner for a job on a cold queue.
- A run whose jobs ran on both machines is not [re-run](../cancel-rerun/) for a runner fault on the other one: each machine re-runs only what its own runner proved.
- The time an overflow host leaves a job to the others counts as its own cause of the job's wait, and sizing does not read it.
- Give the machines different [names](../../reference/configuration/#host), so notifications and the dashboard say which one speaks.
- To see the laptop from the desktop's `runnerctl` as well, run `runnerctl machine add laptop --ssh laptop` on the desktop. That is for watching only: the machines still share no state.
