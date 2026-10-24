# 0056. A runner a restart adopts keeps what it reserved

Status: **Accepted** (2026-10-24). Amends [0033](0033-measured-admission.md).

## Context

ADR 0033 had a runner a restart adopts reserve its cap: the daemon did not know what it had reserved. The runner's
record has kept that since, rewritten each time the reservation follows its queue or its job. Under
`[host] cpu_overcommit` the cap costs the most. CPU has no sample that lowers a reservation, so an adopted
runner held its whole CPU limit until its job ended. On the owner's 48-CPU host, a restart that adopted four
runners counted 92 CPUs reserved (31 + 31 + 15 + 15) of a budget of 48, and no runner started for CPU until they
ended.

## Decision

- Recovery reads each adopted runner's admission from its record. The runner reserves what it reserved before
  the restart, and the reservation follows its job's prediction again, as for any other runner.
- A runner whose record has no admission, as one started by an older build, still reserves its cap.
- Memory keeps its guard: a runner without a sample from the last three intervals counts at its cap, and the
  samples of a runner are not kept across a restart, so an adopted runner counts at its memory cap until its first
  sample.

## Consequences

- A restart no longer holds jobs for CPU that the runners it adopted do not use.
- An adopted runner's record says how it was admitted, not `None`.
