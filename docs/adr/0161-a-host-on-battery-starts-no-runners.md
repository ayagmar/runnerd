# 0161. A host on battery starts no runners

Status: **Accepted** (2026-11-23).

## Context

runnerd is meant to run on a laptop as well as on a desktop. A CI job on battery drains it in minutes: a Gradle or
Docker build keeps every core busy. The owner should not have to pause each repository by hand when they unplug, nor
remember to resume them.

How to tell a laptop is on battery needs care. "Mains online" is not enough: a laptop that charges only over USB-C
lists a `USB` supply and no `Mains` one, and would read as on battery for good. A wireless mouse lists a battery of
its own. A battery held at a charge limit says `Not charging` while the laptop is plugged in.

## Decision

- `[host] on_battery = "run" | "pause"`, default `pause`. A reload applies it at the next tick.
- The host is on battery when a system battery under `/sys/class/power_supply` (`type` `Battery`, `scope` absent or
  `System`) says `status` `Discharging`. `Not charging`, `Full` and `Charging` are mains power. No `Mains` supply is
  read. A device's battery (`scope` `Device`), a supply whose files cannot be read, and a host with no system battery
  never put the host on battery, so a desktop never pauses. `observe::power` reads the files.
- The engine reads them once a tick, before its plan; a pass between ticks plans with the last reading. Nothing is
  stored: a restart or a reload reads again at its first tick.
- While on battery under `pause`, the plan gives every repository `scheduler::Pause::Battery`, as it gives a paused
  one `Pause::Repository`: no runner starts, idle and warm runners stop at once (`StopReason::OnBattery`, stored as
  `on_battery`), and busy runners finish their jobs. A start already planned does not launch. The owner's pause wins
  where both hold, as it outlasts the battery. The stored pause is untouched, so plugging in resumes exactly what was
  running before.
- Queued jobs wait `api::Waiting::OnBattery`, which core words as "The host is on battery; runners start once it is
  plugged in". It is `WaitKind::Act` and not `cannot_start`: it ends by itself, so it adds no attention item. Its
  wait is counted under `WaitCause::Battery` (`battery`).

## Consequences

- 0.1.4 and 0.1.5 clients read `on-battery` as `Waiting::Unknown` and say the job waits for a reason they do not
  know yet; a downgraded daemon reads a stored `battery` wait as `other`.
- A laptop on battery with no queued jobs shows nothing about the hold beyond its log line; the status carries it only
  through the jobs that wait.
- Auto sizing under `measured` admission counts only the wait with no slot free while the budgets had room
  (ADR 0090), which a battery hold never adds to. Under `caps` it counts the whole wait, so time held on battery reads
  as waiting there, as time paused does.
- Reseeds, measures, image pulls and cache trims still run on battery. They are short next to a job, and holding them
  would leave the slots unready when the host is plugged in.
