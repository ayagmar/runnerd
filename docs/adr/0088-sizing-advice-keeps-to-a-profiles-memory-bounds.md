# 0088. Sizing advice keeps to a profile's memory bounds

Status: **Accepted** (2026-10-30). Amends [0086](0086-sizing-and-init-know-the-memory-ceiling.md), [0039](0039-auto-sizing.md).

## Context

ADR 0086 bounded a profile's advice by `[host] memory_ceiling` but left `min_memory` and `max_memory` to auto
sizing, which clamped its own change after reading the advice. Everything else that reads the advice applied it as
it was. `runnerctl sizing apply` and the dashboard's config editor wrote a raise past `max_memory`, such as 11.75G
over an 11G `max_memory`, or a lowering under `min_memory`, and the config check refused it. The advice line, the
TUI and the dashboard told the owner to raise to a size the config forbids, and a profile at its `max_memory` whose
jobs needed more read as a raise to make at every look.

## Decision

- **One target, in core.** `sizing::advise` bounds a profile's advice by every bound of the config the check holds
  it to: a raise and a revert by the lower of what the ceiling leaves one runner (rounded down to 256M, ADR 0086)
  and `max_memory`, and a lowering by `min_memory`. Where the ceiling and `max_memory` give the same size, the
  ceiling is named, since raising `max_memory` alone would not let a raise past it. The slots are fitted at the
  bounded size, and the slots a lowering adds or a revert gives back stop at `max_slots`. Auto sizing,
  `runnerctl sizing apply`, the dashboard's editor and the TUI all write `Advice::change` as it is; auto sizing no
  longer clamps `memory` or a lowering's slots itself, and moves a lowering by at most `max_step`. The one slot it
  adds on its own for jobs that waited still stops at `max_slots`, since that is not the advice's.
- **What the jobs need stays visible.** `ProfileSizing::hold` replaces ADR 0086's `ceiling`: a `MemoryHold` with
  what the jobs need, the bound the advice goes to, and which bound it is (`MemoryBound`: the ceiling with the
  mirror beside it, `max_memory` or `min_memory`). It is set only when a bound moves the advice away from the need,
  and is worked out from the config at each look; nothing is stored. Core words each case:
  - a raise held short: `raise memory to 11G, its max_memory, short of the 12.75G its jobs need: …`
  - a profile at its `max_memory` whose jobs need more: `Advice::SliceTooSmall`, which writes nothing and has tone
    bad, with `keep 11G, its max_memory: its jobs need 12.75G (…); raise max_memory to give them more`,
    `at max_memory` in a table cell, and a doctor fix that raises `max_memory`. `ProfileSizing::held` says which
    bound holds it, the ceiling or `max_memory`.
  - a lowering `min_memory` holds: `lower memory to 8G from 10G, its min_memory: its largest peak + 1G for page
    cache is 5G`, and once there, `keep 8G, its min_memory: …`, tone good.
  - a revert held short: `raise memory back to 12G, its max_memory: …`. Like one the ceiling holds, it is written
    at the next look and finishes once a reload raises the bound (ADR 0086's chain).
- **`Sizing::need` reads the hold**, so the dashboard's mark shows what the jobs need, not the bound.
- Whether a lowering is worth making is still judged by what the jobs need; `min_memory` only stops it short.

## Consequences

- Every config a client or auto sizing writes from the advice passes the check, and a profile held at a bound
  proposes nothing until a reload moves it.
- `max_memory` under the profile's `memory` is refused by the config check, so the advice never starts from one.
- `ProfileSizing.hold` is a new optional field and `MemoryBound` a new type. Neither was in a release, and v0.0.23
  clients ignore the field: they show a profile held at its `max_memory` as one whose slice has to grow.
