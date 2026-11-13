---
title: runnerd simulate
description: Every option of `runnerd simulate`, as its `--help` prints them.
---

<!-- Written by site/scripts/reference/cli.py from `runnerd --help`. Change the help, then run it. -->

Runs ephemeral GitHub Actions runners in containers within a host budget

The other commands are on [runnerd](../runnerd/).

## `runnerd simulate`

Replay the job history through admission by caps, by measured peaks and with a CPU budget, and print what each would have done.

Reads the history database read-only; changes nothing.

- `--config <CONFIG>`: The config to replay with [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml]
- `--memory-max <MEMORY_MAX>`: Replay against this MemoryMax (such as 33G) instead of reading it from systemd
- `--since <SINCE>`: Replay the jobs queued in the 30m, 12h, 7d, 2w, … up to the history's last job [default: 30d]
- `--slots <SLOTS>`: The slots a profile may use beyond its configured ones, as PROFILE=SLOTS [default: as many as the slice holds of the profile alone at its cap]
- `--cpus <CPUS>`: Replay against this many CPUs (such as 32 or 7.5) instead of reading the slice's from systemd
- `--cpu-overcommit <CPU_OVERCOMMIT>`: Replay a CPU budget of the slice's CPUs times this; repeat it to compare factors [default: the config's `cpu_overcommit`, else 1]
- `--priority`: Also replay the config's policy and slots, `--slots` over them, with no barrier for a starved job, a strict one and the daemon's, pool by pool and run by run
- `--causal`: Also replay the config's policy and slots as the jobs really ran, each using its recorded samples from its replayed start, with the plain count and with pooled credits of 4G, 6G and 8G: waits, runs and hazards, beside what the same jobs waited as they ran
- `--cold`: With --causal, give every run of a job the samples of its heaviest run, placed so that the run reaches its peak: every cache cold at once
- `--overhead <OVERHEAD>`: With --causal, what the slice holds beside its runners, as the hazards read it (such as 8G) [default: the p99 of the window's busy moments]
- `--guard-overhead <GUARD_OVERHEAD>`: With --causal, what the guards read of that when they decide (such as 1G) [default: --overhead]: a slice that reads low at a decision and high once the job runs
- `--credit <CREDIT>`: With --causal, the credit of a row, 0G for the plain count; repeat it for more rows [default: 0, 4G, 6G, 8G]
- `--count <COUNT>`: With --causal, how rows count what runs: `plain`, by the credits alone, a row for each --credit; `growth`, as the daemon does (ADR 0130); `top2` or `top3`, a row by what runs uses with room for its two or three largest growths, at least 6G, for both MemoryMax and the host, and the plain count while the host has under its margin and a quarter of its memory available, which the options below change. It picks the rows: list `plain` too to keep the credit rows [default: plain]
  - `plain`: Every runner's take, less the credits
  - `growth`: As the daemon does: the growth count with its brake and every rule (ADR 0130)
  - `top2`: What runs uses, with room for its two largest growths
  - `top3`: What runs uses, with room for its three largest growths
- `--brake`: With --causal, give the top2 and top3 rows the growth brake: their count rests for 30 minutes once what runs grew past the room it kept within 10 minutes
- `--bound`: With --causal, bound the top2 and top3 rows' credit as the daemon's is: MemoryMax's at most the slice's room under its high-water mark less 2G, and both at most what the host has past what it keeps
- `--young`: With --causal, make the top2 and top3 rows keep room for at least every young runner's growth: one whose job has run under a minute, or that has no job yet
- `--credits-below`: With --causal, count the top2 and top3 rows by the credits, not the plain count, below the guard's line and while the brake holds
- `--guard-hold`: With --causal and --brake, rest the top2 and top3 rows' count for 5 minutes from a plan that finds it below the guard's line, and again from the first plan after that still below it
- `--ceiling <CEILING>`: With --causal, keep the top2 and top3 rows' credits, MemoryMax's and the host's, at most this
- `--host-reserve <HOST_RESERVE>`: With --causal, what the host's limit reads of what runs: `top2`, eligible runners at their use with room for the two largest growths, as the daemon does; or `plain`, every runner's take
  - `plain`: Every runner's take
  - `top2`: Eligible runners at their use, with room for the two largest growths

  [default: top2]
- `--any-waiter-band`: With --causal, hold the host's larger margin while any job waits for the host, not only while none fits the plain margin as the daemon does: what that gives
- `--shift <SHIFT>`: With --causal, move the host's other programs this much later, round the window (such as 90m): a desktop's burst meeting other jobs than it met
- `--move <MOVES>`: Replay a job in another profile than the history has, as REPOSITORY:JOB=PROFILE: pools made since it ran
