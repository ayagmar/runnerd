# 0101. The host's reading includes its swap, and admission ignores it

Status: **Accepted** (2026-11-06). Amends [0055](0055-admission-follows-the-hosts-memory.md).

## Context

Admission counts `MemAvailable`, which already rises as the kernel swaps. Whether the host's swap room is worth
counting on top is a question for data, and the data does not exist: runnerd records the host's memory only as the
reading in each `budget.changed` event, which has no swap. The few readings taken by hand show the desktop's swap
mostly full on a heavy evening, a quarter of the anonymous pages it swapped out were read back, and a full swap is
where a stalled desktop and `systemd-oomd` begin. A credit for swap room, taken now, would spend room nobody can say
was there.

## Decision

- **`HostMemory::swap`**, `{ total, free }` in bytes, from `SwapTotal` and `SwapFree` in the `/proc/meminfo` read
  that gives `MemAvailable`. Absent when `SwapTotal` is 0 or missing, or `SwapFree` is. The device zram and zswap
  present counts as any other.
- **The time series comes with the events.** `budget.changed` events carry the host's reading, and are stored, so
  swap is recorded as the ceiling is (0084), with no schema change. The field is skipped when absent, so a reading
  from before it deserializes as no swap.
- **Core words it on the host line**, `host: 31G of 64G available, swap 12G of 15.5G used; leaves the slice 40G`
  (`Budget::host_line`). The dashboard, the TUI and `runnerctl status` show that line, and the fixtures draw it.
  Nothing says it on a host without swap.
- **Admission uses it for nothing.** `admission::host`'s limit, margin, stall rule and spare are as before, and a
  test holds them equal with and without a swap device.
- **0.0.28 reads it.** Its `HostMemory` ignores the field; the wire check passes with no accepted break.

## Consequences

- After a week that holds a heavy evening, the recorded readings say how much swap was free while jobs waited for
  the host. If `SwapFree` at the 10th percentile of that time is 6G or more, a credit of at most 2G is worth
  bringing to the owner; if not, the question is closed.
- No new event: swap is part of the reading `budget.changed` already carries.
- Swap is the desktop's: runners get `memory_swap` equal to `memory`, so none of it is theirs, and the figure is
  context for the owner, not a budget.
