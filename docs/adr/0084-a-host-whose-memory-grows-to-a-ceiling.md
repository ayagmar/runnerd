# 0084. A host whose memory grows to a ceiling counts what it can still grow by

Status: **Accepted** (2026-10-30), amended by [0086](0086-sizing-and-init-know-the-memory-ceiling.md). Amends [0055](0055-admission-follows-the-hosts-memory.md).

## Context

ADR 0055 bounds new runners by what the slice's runners hold plus `MemAvailable`, less a margin of the larger of 2G
and a sixteenth of `MemTotal`. On a host whose memory is fixed, `MemTotal` and `MemAvailable` say what it has. On a
VM whose hypervisor hands memory out as the guest asks for it, they say only what it has handed over so far: Hyper-V
Dynamic Memory shrinks an idle guest's `MemTotal` and grows it again when something inside it allocates. A balloon
that leaves `MemTotal` at the maximum and shows ballooned pages as used, such as virtio-balloon with free page
reporting or deflate-on-OOM, is not this case, and the key below does nothing there.

On the owner's Ubuntu 24.04 VM under Hyper-V, with Dynamic Memory from 2G to 24G and one `node` profile of 8G
(#159), the idle VM had 2.12G with 1.48G available, so the host had nothing to spare past its 2G margin and a
queued job waited forever: a queued job allocates nothing, so the VM never grew. A throwaway allocation grew it to
12.8G, the job then started and ran, and within about 3 minutes of its end Hyper-V took the memory back (3.7G after
2.5 minutes, 3.3G after 8). The workaround is an 11G minimum, which holds 11G of the hypervisor's RAM all the time,
what Dynamic Memory is there to avoid.

The guest can sometimes read its ceiling. Hyper-V's balloon driver shows `max_dynamic_page_count` in
`/sys/kernel/debug/hv-balloon` (6291456 pages, 24G, on that VM), but debugfs needs root and runnerd runs as the
user. virtio-balloon tells the guest how many pages to give back, not how far it may grow again, and other
hypervisors show the ceiling another way or not at all. And a ceiling read from the guest is
a promise, not memory: a hypervisor short of RAM gives less than it promised.

## Decision

- **`[host] memory_ceiling`.** An optional size: the most memory the host can grow to. Unset, admission is ADR 0055's
  exactly. Set above `MemTotal`, the host's bound counts what it can still grow by, `memory_ceiling` less `MemTotal`,
  as available: the slice may hold what its runners hold plus `MemAvailable` plus that, less the margin of the
  ceiling, the larger of 2G and a sixteenth of `memory_ceiling` (`admission::host::limit`). The margin is the host's
  memory as configured, since that is the host the runners will run on once it grew. At or under `MemTotal`, the
  ceiling has no effect: the host already has at least that. `MemTotal` leaves out what the kernel reserves, about
  2% (its page tables for the memory, the kernel itself), so a VM grown to a 24G maximum reports about 23.5G: the
  ceiling to set is `MemTotal` read at the VM's maximum, or the maximum less 2%, or admission keeps counting growth
  that never comes. Under `MemTotal`, `runnerd doctor` warns, since that ceiling is a mistake.
- **The stall rule stays as it is.** Nothing starts while `full` pressure over the last 10 s reaches 5% with
  `MemAvailable` under twice the margin of `MemTotal`, until it falls under 1%. Neither figure counts the ceiling:
  what the host could grow by is not available yet, and a host that stalls while it should be growing is one whose
  hypervisor is not giving what the ceiling promised. That is the guard against a wrong ceiling. The half margin more
  after a wait and while a stall lasts applies to the ceiling's margin.
- **The ceiling is read from the config, not stored in the history.** The engine adds it to each reading of the
  host it plans by (`HostMemory::ceiling`), so a reload that changes it applies at the next plan.
  `HostMemory::grown` and `growable` say the host's memory as admission counts it, and the host's line says it:
  `host: 1.48G of 2.12G available, growing to 24G; leaves the slice 21.36G`. A job that waits for the host says
  what the grown limit leaves, not 0B.
- **`runnerd init` and `runnerd doctor` read the host by the same rule**, `admission::host::first_runner` on a
  reading that carries the ceiling: init sizes a reading with a ceiling for the host as it can grow, three quarters
  of the ceiling and no more than what admission then leaves its first runner. Init takes the ceiling as
  `--memory-ceiling` ([0086](0086-sizing-and-init-know-the-memory-ceiling.md)); without it, it reads the host as it
  is now. The doctor applies the config's
  ceiling and says it is in use (`…, growing to 24G, 20.86G to spare for runners past a 2G margin…`). Beside any
  other warning, it warns when the ceiling is strictly under `MemTotal`, where it has no effect.
- **The config check** refuses a ceiling of 0, and one under the memory of the largest profile's runner beside the
  mirror: no host of that size starts one, so it is a typo such as `24M` for `24G`. It does not compare the ceiling to
  `MemTotal`, which it does not read, nor to `MemoryMax`: a slice smaller than the grown host is a choice.
- **A key, not detection.** Reading the ceiling needs root on Hyper-V and differs per hypervisor; a key works on all
  of them, and says what the owner configured rather than what the guest guesses. Detection can come later, per
  hypervisor, as a default for the key.

## Consequences

- An idle VM with a ceiling starts its first job: the job's allocations grow the VM, as the throwaway allocation did.
  The hypervisor's minimum can go back to what the host needs idle.
- A ceiling higher than the hypervisor delivers brings back what ADR 0055 prevents: starts the host has no room for,
  reclaim, and throttled sockets, until the stall rule stops further starts; the starts of one plan all go ahead, and
  the guest's OOM killer can kill a job among them. runnerd still never stops a running job. A
  ceiling should be `MemTotal` at the VM's maximum, and lower when other VMs on the same hypervisor compete for its RAM.
- Growing takes the hypervisor a moment, while a runner starts against memory not there yet. The stall rule holds
  further starts while it lasts.
- With a ceiling, the margin is larger than a sixteenth of the host as it is now: 6G on a 16G VM that can grow to
  96G. That is what the margin will need once it grew.
- Auto sizing bounds a memory raise by the ceiling less the mirror as well as by `MemoryMax`
  ([0086](0086-sizing-and-init-know-the-memory-ceiling.md)), so it never proposes a raise the config check refuses.
- `HostMemory.ceiling` is a new optional field, absent unless set; an older client ignores it and shows the host's
  line without `growing to`.
