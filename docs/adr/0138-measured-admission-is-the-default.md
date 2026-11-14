# 0138. Measured admission is the default

Status: **Accepted** (2026-11-13). Amends [0033](0033-measured-admission.md).

## Context

[ADR 0033](0033-measured-admission.md) put admission by measured peaks behind `[host] admission = "measured"` and
kept caps as the default. Since then measured admission learned young jobs ([0036](0036-young-jobs-predicted-from-their-own-runs.md)),
the host's memory ([0055](0055-admission-follows-the-hosts-memory.md)), adopted runners
([0056](0056-adopted-runners-keep-their-reservations.md)), recent runs ([0070](0070-predictions-follow-recent-runs.md)),
starved jobs ([0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md)) and the growth count
([0130](0130-the-budgets-count-what-runs-uses-and-keep-room-for-its-largest-growths.md)), and the owner's host runs it.
Under caps a runner holds its profile's heaviest job's memory whatever job it runs, so jobs wait for memory the running
ones never touch. Features ship on by default, with an opt-out, and the end-to-end run proves them.

## Decision

`[host] admission` defaults to `"measured"`. `"caps"` stays, as the opt-out.

- A config without the key gets measured admission: when the daemon starts, at every reload, in `runnerd check-config`
  and in the dashboard's check. The config check then holds one runner of each profile with the mirror to
  `MemoryMax`, not `slots × memory` over all of them, and refuses a slice without a `MemoryMax`. Its message names
  both ways out: the slice's unit file, which `runnerd doctor` prints, or `admission = "caps"`.
- `runnerd init` writes `admission = "measured"` with a comment naming `"caps"`, so the file says what runs. It writes
  the config before the slice exists, so it checks it against the `MemoryMax` its slice fix gives the slice.
- `runnerd doctor` on a slice without a `MemoryMax` under measured admission fails the slice and checks the config
  against the `MemoryMax` the slice's fix sets: a config error shows now, and a config that passes is skipped as
  needing that `MemoryMax`, so one cause gives one failure. Under measured admission that fix sets every cap at once
  but no more than `runnerd init` gives the host (`init::budget`), since the caps may add up past it. Under caps the
  doctor is unchanged.
- On a host with no history a job reserves its cap until its profile has `min_jobs` runs (0033), so the first jobs
  start no more than caps would have started, and none past `MemoryMax`.
- A reload that removes `admission = "measured"` changes nothing: every runner keeps what it reserved. One that sets
  `"caps"` counts every runner at its cap and stops none, as before.
- `runnerd simulate`'s table replays both policies as before; `--priority` and `--causal` replay the config's policy,
  measured when the key is absent.
- `examples/runnerd.toml` sets `admission = "measured"` with a comment naming the opt-out. The dashboard's demo keeps
  its base scenario under caps, which its budget and sizing fixtures are drawn for, and its `measured` scenario shows
  the default.

## Consequences

More jobs run at once on the same memory when their peaks sit well below their caps. ADR 0033's risk now comes with the
default: several jobs past their predictions at once can fill the slice, and the kernel then kills a process in it.
Each container's cap, the 90% high-water mark and the growth count still bound it, and a job killed for memory goes
back on its cap.

A slice without a `MemoryMax` stops the daemon from starting where caps only warned. `runnerd init` and `runnerd
doctor` already printed the slice's unit file as a required step; a host that cannot have one sets `admission =
"caps"`.

The wire format is unchanged: the budget, the config check and every job's record already name the policy.
