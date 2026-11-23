# 0158. A host has a name that its status and messages carry

Status: **Accepted** (2026-11-23). Adds `[host] name`, `config::host_segment`, `api::Machine`, `Status::machine`,
`Notification::machine`, `HostIdentity::machine`, and changes the host segment of runner names.

## Context

A second machine may serve the same repositories, each with its own daemon. Its runners are told apart by
`runnerd-<id>-`, the 8 hex digits hashed from the systemd machine ID (`HostIdentity::prefix`), which stays the only
test of whose runner a registration is. Three things still made two machines hard to tell apart:

- A message from `[notify]` and a dashboard tab read the same on both machines.
- Nothing showed the 8-hex id, so two installs that share a cloned machine ID, and so take each other's runners for
  their own, could only be found by reading runner names on GitHub.
- The host segment of `runnerd-<id>-<host>-<profile>-<slot>-<secs>` kept dashes, and so may a profile name, so
  `laptop-dhcp-42-my-app-1-…` cannot be split into its host and profile.

## Decision

- **`[host] name`.** Lower-case letters and digits, 1 to 20 of them; the check refuses anything else and suggests
  the name with only its letters and digits. Absent, the name is the kernel's host name with only its letters and
  digits, in lower case, at most 20; empty, `host`. It is the host segment of every new runner name.
- **The host segment holds no dash.** It is the text after `runnerd-<id>-` up to the next `-`. The prefix is
  unchanged, so the runners a 0.1.5 daemon started, with their dashed host segment, still read as this machine's
  after an upgrade or a rename, and are adopted or removed as before.
- **`Status::machine`** carries the id and the name (`api::Machine`), so a client can show which machine it reads and
  a person can compare the ids of two machines. Absent from an older daemon.
- **Messages and pages name the machine.** Every `[notify]` message's title ends with `on <name>`
  (`Machine::notification_title`), and a webhook's JSON carries `machine`. Every dashboard page's title reads
  `<page> · runnerd on <name>` (`Machine::page_title`); the dashboard reads the machine from the status once.
- **Read once at start.** The engine's identity is fixed when the daemon starts. A reload that changes `[host] name`
  renames nothing until the next start; until then the status, messages and runner names keep the old name.

## Consequences

- Two daemons sharing one machine ID are found by comparing `machine.id` in `runnerctl status --json` on each.
- A peer still on 0.1.5 names its runners with a dashed host segment, so parsing a host out of its runner names is
  wrong until both machines run this version. Nothing parses it yet; the docs say to upgrade both machines.
- A configured name that is not already its own host segment fails the check rather than being changed silently, so
  what the config says is what the runner names, messages and dashboard show.
