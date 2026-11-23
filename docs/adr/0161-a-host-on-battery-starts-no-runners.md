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
- The engine reads them before each plan that may start runners: a tick's, and a pass's between ticks, so a runner
  that ends just after the unplug frees no slot for a new one. Nothing is stored: a restart or a reload reads again at
  its first tick.
- While on battery under `pause`, the plan gives every repository `scheduler::Pause::Battery`, as it gives a paused
  one `Pause::Repository`: no runner starts, idle and warm runners stop at once (`StopReason::OnBattery`, stored as
  `on_battery`), and busy runners finish their jobs. A start already planned does not launch. The owner's pause wins
  where both hold, as it outlasts the battery. The stored pause is untouched, so plugging in resumes exactly what was
  running before.
- Queued jobs wait `api::Waiting::OnBattery`, which core words as "The host is on battery; runners start once it is
  plugged in". It is `WaitKind::Act` and not `cannot_start`: it ends by itself, so it adds no attention item. Its
  wait is counted under `WaitCause::Battery` (`battery`).
- The status says the hold whether or not a job is queued: `api::Status::on_battery`, absent while false, which
  `Status::battery_line` words and `runnerctl status`, `runnerctl watch`, the TUI's header and the dashboard's overview
  draw. Without it, warm runners that stop on an idle laptop would have nothing that says why.
- Time a job was held on battery is not a wait for a runner: `sizing::Wait::held`, the job's `battery` cause, comes
  off `Wait::counted`, and off the SQL that counts the same for the dashboard's and the stats' waits. So the waits
  insight, the warm and cold medians, and sizing under `caps` leave it out; the wait-cause split still shows it as its
  own cause. A job's own page keeps its whole wait.
- While the hold is on, a repository is listed at the idle pace (`poll_interval`) whatever it has queued: a job that
  cannot start does not need three listings a `poll_interval`, on the battery the hold spares. A plug-in is noticed
  within a `poll_interval`.
- A re-run's preview says the new attempt's jobs wait for mains power, from the hold the engine mirrors into
  `Control` at each read.

## Consequences

- 0.1.4 and 0.1.5 clients read `on-battery` as `Waiting::Unknown` and say the job waits for a reason they do not
  know yet; a downgraded daemon reads a stored `battery` wait as `other`.
- 0.1.5 clients do not show the status's hold; they still show each queued job's reason as one they do not know.
- Time a repository was paused still counts as waiting, as before: a pause is the owner's act and rare, and its
  time is not recorded apart from other waits in older rows.
- Reseeds, measures, image pulls and cache trims still run on battery. They are short next to a job, and holding them
  would leave the slots unready when the host is plugged in.
