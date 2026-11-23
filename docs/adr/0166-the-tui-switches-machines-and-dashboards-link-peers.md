# 0166. The TUI switches between machines, and dashboards link to their peers

Status: **Accepted** (2026-11-23). Adds `m` and the list of machines to `runnerctl tui`, the machine's name to its
header, `[web] peers` (`config::Peer`) and the dashboard's links to them.

## Context

`--machine` (ADR 0164) picks a machine when `runnerctl tui` starts, and `runnerctl fleet` (ADR 0165) shows every
machine at once, but in the TUI a person had to quit and start again to look at another machine, and nothing on
screen said which machine it showed. Each machine serves its own dashboard on its own address; a client file
(`machines.toml`) is no help to the daemon, which serves the pages.

## Decision

- **The list of machines is the TUI's fleet overview.** `m` reads this machine's daemon and every machine of
  `machines.toml` at once, as `runnerctl fleet` does, and lists each with `FleetMachine::summary` in
  `FleetMachine::tone`: a machine that is down reads in the bad tone with why. Enter on another machine reads it.
- **A switch replaces everything at once.** The loop opens the new client and reads its snapshot in a task while
  the old machine stays on screen. Only once that read succeeds does it stop the old event feed and log reader and
  swap the client and the state. A switch that fails leaves the old machine on screen and flashes why. What the
  daemon sends comes on one channel per machine, dropped with what is still in it on a switch, so a late answer from
  the machine left never reaches the next one's state. Keys keep their own channel.
- **The header names the machine** it reads, ` runnerd on skiff `, from the daemon's status
  (`Machine::notification_suffix`); a 0.1.5 daemon names none.
- **`[web] peers = [{ name, origin }]`.** Names follow `[host] name`'s rule; origins are `config::Origin`, checked
  as `[web] origins` are. A peer that is one of `origins` (this dashboard itself), or a name or origin two peers
  share, fails the check. Read when the daemon starts, as `origins` is. Every page's header links to each peer's
  dashboard by name; the link signs nobody in.
- **No merged dashboard.** The dashboard shows its own machine; the links only lead to the others.

## Consequences

- The list of machines opens one SSH forward per added machine each time `m` is pressed.
- `runnerd_web::spawn` and `serve` take the peers beside the origins.
- A reload does not change the peers; a restart does.
