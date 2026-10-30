# 0086. Auto sizing and `runnerd init` know the memory ceiling

Status: **Accepted** (2026-10-30). Amends [0084](0084-a-host-whose-memory-grows-to-a-ceiling.md).

## Context

ADR 0084 added `[host] memory_ceiling` for a VM whose memory grows on demand. The config check refuses a ceiling
under one runner of the largest profile beside the mirror. Auto sizing did not read it: a raise of `memory` was
bounded by the slice's `MemoryMax` and the profile's `max_memory` only. Where the ceiling is under `MemoryMax`, a
raise past the ceiling less the mirror failed the check, and the next look proposed it again, and every look after.
The same held for the revert of a lowering that went too far, which goes straight back to the `memory` it came
from: a reload that lowered the ceiling since could leave that `memory` past it. The advice `runnerctl sizing`
shows had the same raise, so `runnerctl sizing apply` wrote a config the check refused.

`runnerd init` read the host as it is now. On an idle Hyper-V guest with 2.12G, it refused to write a config, and
the owner had to write `memory_ceiling` by hand before anything could start.

## Decision

- **One bound, read by the check and by sizing.** `Config::runner_ceiling` is the ceiling less the mirror while it
  is enabled: the most `memory` one runner may have. The config check refuses a profile above it. The advice raises
  `memory` no further than it, rounded down to 256M, since a config file says a size with two decimals and a value
  between granules would be read back as something else, perhaps over the bound. A revert goes back as far toward
  the `memory` it came from as the bound allows, still at the next look. The bound sits beside the others, not in
  place of them: under `caps` the slots still have to fit `MemoryMax` beside the other profiles, under `measured` one
  runner beside the mirror, and `max_memory` still applies. It bounds one runner, not all of a profile's slots: the
  ceiling is what the host can grow to, and `MemoryMax` is what bounds the slice.
- **Read from the config at each look.** Nothing about the bound is stored: a reload that raises or removes the
  ceiling, or turns the mirror off, lets the next look raise further.
- **What the jobs need stays visible.** A profile's advice is the raise that can be written; `ProfileSizing::ceiling`
  says what the jobs need and what holds the raise. A raise the bound cuts short says so (`raise memory to 11.75G,
  the most the host's 12.4G memory_ceiling leaves one runner beside the mirror's 512M, short of the 13.25G its jobs
  need: …`). A profile already at the bound gets `Advice::SliceTooSmall`, which writes nothing and is a raise its
  jobs need, with a line of its own (`keep 11.75G, the most …: its jobs need 13.75G (…); raise [host]
  memory_ceiling if the host can grow further`), `at ceiling` in a table cell, and `runnerd doctor`'s fix grows
  the ceiling, not the slice. It says no raise is possible, not that the jobs fit. Each job's own advice is not
  bounded: it says what that job needs. Core words all of it; clients show it.
- **`runnerd init --memory-ceiling <size>`** writes `memory_ceiling` under `[host]` and sizes the first runner on a
  reading that carries it, by `admission::host::first_runner` as the doctor does: three quarters of the ceiling, and
  no more than what admission leaves the first runner on the host as it can grow, less init's startup headroom,
  within the slice's `MemoryMax`. It reads the size with the config's parser and writes it as given. It refuses 0 with
  the config's words before it sizes anything, and runs the config check on the config it would write before it
  writes either file, so a ceiling under one runner and the mirror, such as 4G on a 64G host where it counts for
  nothing in admission, is refused with the check's words and nothing is written or replaced. Without the flag its
  output is what it was, byte for byte.

## Consequences

- An elastic VM can be set up with `runnerd init` alone, and auto sizing on it never writes a config the check
  refuses, so it no longer logs the same refusal at every look.
- A profile held at the bound whose jobs need more keeps running at the bound; its jobs that need more can still
  run out of memory. Only a larger ceiling, which the hypervisor has to deliver, fixes that.
- The ceiling stays a promise: sizing for it does not make the memory appear. A hypervisor that gives less still
  stalls the host and can OOM a job, as ADR 0084 says.
- `ProfileSizing.ceiling` is a new optional field, absent unless the ceiling holds a raise. An older client ignores
  it and shows a profile at the bound as one whose slice has to grow.
