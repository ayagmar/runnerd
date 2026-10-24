# 0048. The slice's charts reach up to the slice

Status: **Accepted** (2026-10-24).

## Context

The overview's CPU chart was scaled to the sum of every slot's CPU limit, and its memory chart to `MemoryMax`
rounded up to the next round size. On the owner's 32-thread host, two 12-CPU and ten 8-CPU slots drew a "104 CPUs of
all slots" line on an axis of 200 cores, and 33G of `MemoryMax` an axis of 80G: the slice's whole looked like a small
part of the chart. Slot CPU limits are quotas that may add up past the host; they are not a ceiling.

## Decision

`api::Budget` gains `slice_cpus`: the CPUs the slice may use, in millicores, as CPU admission reads them. The daemon
reads them at start and with each config applied, whether or not `cpu_overcommit` is set; without it, a failed read
leaves them out and refuses nothing. The memory chart's axis tops at `MemoryMax` and the CPU chart's at
`slice_cpus`, or at a rounded-up peak when they are unknown. The limits are named in the axis and the charts' titles,
not as a line at the top edge.

## Consequences

- An older daemon leaves `slice_cpus` out; the CPU chart then scales to its peak.
- Without `cpu_overcommit`, runnerd now runs `systemctl show` for the slice's CPUs at start and on each config
  applied, as it already does for `MemoryMax`.
- Rejected: keeping the slots' sum as a line (it is not a limit), and showing it as text (nothing on the overview
  acts on it; the config page shows each profile's CPUs).
