# 0114. A lifecycle is one value with one owner

Status: **Accepted** (2026-11-08). No wire, schema or config change.

## Context

The engine keeps most lifecycles as loose fields: `Occupant.busy` beside `job` and `job_started`, a job's progress
across `queued_jobs`, `closing` and `claims`, a profile's upkeep across nine maps with one key. Each field is right
alone. What a combination means lives in conditions spelled at each call site, and combinations nobody meant can be
written. Bugs in this area keep having the same shape: a call site that forgot one of the fields.

## Decision

- **A lifecycle is one value**: an enum, or a struct whose fields are all independent, owned by one type. Data of one
  state lives in that variant.
- **Only methods of the owning type, in its leaf file, write it.** A leaf file has no child modules, so private fields
  make rustc the proof that nothing else writes them. Readers use methods, and a condition over the state is not
  spelled at a call site.
- **The type's doc says what survives a restart and a reload.**
- **`.github/scripts/check-state.py` enforces what a script can**, on non-test Rust. C1: a struct a long-lived owner
  holds, with two or more lifecycle fields, is sealed or at most one of them is written after construction. C2: one
  function does not write the same key in two of `Engine`'s maps. C3: a condition over two fields of one receiver is
  not spelled twice. C4: `Engine` does not gain lifecycle fields. C5: only the store's writer updates the jobs table.
  C6: two machines the others cannot see, `docker_in_stop` and `warm.learned`, are findings by a fixed pattern until
  they are gone.
  It runs its fixtures first, so it fails if it stops catching a case.
- **Exceptions are named.** `MARKED` in the script gives the reason a finding stays. `.github/state-debt.txt` lists
  what is still to fix, with what fixes it. Against `origin/main` a debt line cannot be added, the limit cannot rise,
  and a marker is new only if main tags that debt line `# may become a marker`. `OPAQUE` gains no key and `ROOTS` loses
  no root, as what the walk does not reach is not a finding. If main has the debt file but not the script, or CI did
  not fetch main, the check fails rather than skipping the comparison. The check runs in a job of its own on any change,
  so a pull request that edits only the debt file runs it too.
- **A refactor proves itself with a trace.** The invariants' harness can write, per seed, what a generated case asked
  of GitHub and Docker and its status, step by step, including job reads, inspects and log reads, and the `jobs` table
  it left. It waits for what a step set going before it moves on, and is compared without sorting. A capture's `exec`
  runs in its own task, so where it falls among a tick's other calls is not behaviour: execs get their own section per
  step, in their order.

## Consequences

- The check matches names, not types. It sees through `std::` paths and `Arc`, `Mutex`, `RwLock`, `RefCell` and `Cell`,
  and counts atomics. It cannot see a machine that spans two structs, sits in a tuple (`slot_waiting`), behind a type
  alias, in SQL NULLs, in membership of a `Vec`, or in what a value means, beyond the two named by C6. It does not see a
  write made through a destructured binding (`let State { a, .. } = state; *a = None`) or a guard held in a binding. An
  enum whose transitions are spread over files passes too. A new long-lived root is added to `ROOTS` by hand. Those are
  for review.
- Markers shrink only, so new code seals its types instead of asking for one.
- Tests reach engine state through `engine/tests/access.rs` and `#[cfg(test)]` constructors, so a pull request that
  moves a field changes those bodies and not the tests.
