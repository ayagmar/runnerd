# 0175. The list of machines fills each machine as it answers

Status: **Accepted** (2026-11-24). Amends [0165](0165-one-view-over-several-machines-is-merged-by-core.md) and
[0166](0166-the-tui-switches-machines-and-dashboards-link-peers.md): the TUI's list of machines reads each machine's
status alone and fills its line as it lands, and a machine's line says when its battery holds every start. Adds
`FleetMachine::of_status`, `FleetMachine::this_machine`, `FleetMachine::unasked`, `api::ON_BATTERY`, `Reach::Read`'s
`on_battery` and `tui::listing::Listing`. The daemon's API is unchanged. `runnerctl fleet --json` gains one field,
`on_battery: true` on a machine its battery holds, left out while it is false; a reader that does not know it, as
0.1.6's `Fleet` does not, ignores it, and one that does reads it as false when absent.

## Context

`m` in `runnerctl tui` opened the list at once, then read every machine as `runnerctl fleet` does: each machine's
status and a page of one job, which the list threw away, and no line filled until the slowest machine answered. An
SSH connect may take up to 20 s (ADR 0164), so one laptop asleep left `localhost`, which answered in milliseconds,
reading for 20 s too. A list closed and opened again could also take the first opening's late answer.

A machine on its battery with `[host] on_battery = "pause"` starts no runner (ADR 0161), and `Status` says so, but the
machine's line in the fleet and in the list did not: it looked like any idle machine.

## Decision

- **The list reads each status alone and fills each line as it lands.** `machines::statuses` asks `/v1/status` of
  this machine's daemon and of every machine of `machines.toml` at once, in a `JoinSet`, and the loop sends each
  answer to the reducer as it lands, keyed by the machine's target. The reducer replaces that machine's line in
  place, the cursor kept, so `localhost` and an awake machine fill while one asleep still reads `api::BEING_READ`.
  No machine is asked for jobs. `runnerctl fleet` still reads status and jobs and merges them once all answered.
- **One owner, one read.** `tui::listing::Listing` holds the read behind the open list with a ticket, as
  `Switching` holds a switch (ADR 0114). Opening the list gives up the read before it; the loop gives the read up
  whenever the list is not open, which drops the `JoinSet` and so every client and `ssh` still under way; an answer
  whose ticket is not the read under way's is dropped. The loop lists the machines of `machines.toml` itself when
  `m` asks, before the next message, so the list is open by the time it next checks.
- **Core words the line from a status alone.** `FleetMachine::of_status` builds the same line the fleet's merge does
  from a status, without calling its jobs unread, since none were asked; `this_machine` names this machine as
  `MachineReading::this_machine` does.
- **The battery hold is on the line, first.** `Reach::Read` keeps `Status::on_battery`; `FleetMachine::summary`
  starts with `api::ON_BATTERY`, `on battery, no runner starts; `, before the counts, so a list that cuts the end of
  a line (an 80-column terminal beside a 20-character name) still shows it, colour or not; `tone` warns, in the fleet
  and the list alike. The counts stay the machine's
  own: no fleet-wide queue, no guess of a machine's role from its waits.

## Consequences

- `m` still opens one SSH forward per added machine; closing the list or choosing a machine stops those still
  connecting.
- A daemon before 0.1.6 sends no `on_battery`, and its line says nothing of a battery.
