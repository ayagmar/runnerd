# 0116. A profile's upkeep is one state

Status: **Accepted** (2026-11-09). No wire, schema or config change.

## Context

What a profile's upkeep was doing lived in nine maps and sets of `Engine`, each keyed by profile: `reseeds`,
`reseed_memory`, `reseed_requested`, `reseed_due`, `oversized`, `measure_due`, `measures`, `measured_at` and
`reseeded_at`. That a measure and a reseed never run together was held by filters in three places. Nothing in
the types kept `reseeds` and `reseed_memory` in step, or `oversized` from standing without `reseed_due`.

## Decision

`engine/upkeep.rs` owns one `Upkeep`, a map from profile to its state, with private fields (ADR 0114):

- **One activity:** idle, measuring with its task, or reseeding with its task and its seed container's memory cap.
  The cap can no longer be there without the reseed.
- **One reason a reseed is due**, ordered `Changed` < `Oversized` < `Requested`. A later mark replaces only a weaker
  one, so a measure or a check of the caches cannot turn a reseed the API asked for into one that may skip.
  `Oversized` and `Requested` run however little slot 1 changed.
- `measure_due`, `measured_at` and `reseeded_at` stay beside these, as each can change in any activity.

**A reload keeps the entry.** For a profile it removed, it resets `due` and `measure_due` only. Removing the entry
would drop a running reseed's task and memory, which a retired profile's lifetime depends on, and the times that pace
a profile that comes back.

**A restart keeps only a reseed whose seed container still runs**, which recovery takes back. The rest is lost, as
before.

## Consequences

- A measure and a reseed of one profile cannot run together: both read the one activity.
- A due reseed now also waits while a reseed runs. Nothing could make one due then: the API refuses the request, a
  check of the caches skips a reseeding profile, and no measure runs during a reseed.
- Entries are never removed, so the map holds a name for each profile configured since the daemon started, as
  `measured_at` and `reseeded_at` did.
