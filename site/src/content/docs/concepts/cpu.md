---
title: CPU admission
description: A CPU budget beside the memory one, what a runner reserves of it, and how Linux shares the slice's CPUs.
---

`cpu_overcommit` under `[host]` adds a CPU budget, the slice's CPUs times the factor, and a runner starts only when what it reserves of that fits too:

```toml
[host]
admission = "measured"
cpu_overcommit = 1.0
```

- The slice's CPUs are its `AllowedCPUs`, or the host's, fewer when its `CPUQuota` allows less, read from systemd.
- Under `measured` a runner reserves its job's average CPU, predicted from the history as memory is, with no margin: the factor is the margin. Under `caps` it reserves its profile's `cpus`.

A job that had a slot and memory but not CPU waits; after the same 10 or 2 minutes [it goes first](../growth-and-priority/#a-starved-job-goes-first), by the same rule as memory. The status line says so (`CPU 30 of 32 CPUs reserved; a job waits for CPU`).

Averages are far under peaks, so on a host with many CPUs they admit nearly everything; on 8 CPUs ([Simulating](../../guides/simulate/)) the budget holds jobs. Start at `1.0`: peaks that land together cost a short slowdown, not a kill.

## Sharing the CPUs

Linux shares the slice's CPUs between containers in proportion to each profile's `cpus`, and no container gets more than its `cpus`. A low `CPUWeight` on the slice, such as 20, leaves the desktop its share.
