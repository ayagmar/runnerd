# 0164. runnerctl reaches another machine over an SSH forward

Status: **Accepted** (2026-11-23). Adds `client::Endpoint`, `Client::open`, `client::SshProgram`,
`ClientError::Unreachable`, `config::machines`, `runnerctl machine add|list|rm` and `--machine`.

## Context

A second machine may run its own daemon. Its socket API trusts only its owner's uid on that machine, and a network
API would need its own authentication (ADR 0030 keeps the client on a Unix socket). Reading the other machine's
status meant `ssh skiff runnerctl status`, which needs `runnerctl` on the remote `PATH` of a non-interactive shell
and cannot drive the TUI or a followed log.

`Client` opens one connection per request to a socket path, so a forward of a local socket to the remote one is all
it needs. A connection through `ssh -L` arrives on the remote socket as the owner's uid, which the daemon already
accepts.

## Decision

- **One managed forward per client.** `Client::open(Endpoint::Ssh(machine), …)` starts `ssh -N` with the options
  below, `-o StreamLocalBindUnlink=yes -o ExitOnForwardFailure=yes -o ServerAliveCountMax=3 -L
  <dir>/runnerd.sock:<remote> -- <target>`, and waits up to 20 s for `ssh` to set up the local socket without
  exiting. `<dir>` is a new 0700 directory under `$XDG_RUNTIME_DIR` (else the temporary directory, and so when its
  path has a `:`, which `-L` would split), never reused. Clones share the forward; the last one dropped kills `ssh`
  and removes the directory, and so does an open given up on midway. An `ssh` that exited, as after a network drop,
  is started again on the next request. A connection refused while `ssh` still runs is tried again for 250 ms,
  since OpenSSH creates the socket a moment before it listens.
- **The owner's SSH config says how to reach the host, not how the child behaves.** Their `Host` aliases, users,
  keys, ports and jump hosts apply; `-F /dev/null` would drop them, and they are how a person reaches the machine.
  Every `ssh` runs with `-T -o BatchMode=yes -o ControlPath=none -o ControlMaster=no -o ConnectTimeout=10 -o
  ForkAfterAuthentication=no -o RemoteCommand=none -o PermitLocalCommand=no -o LogLevel=INFO -o
  ServerAliveInterval=15`, and the remote command of `machine add` also with `-o ServerAliveCountMax=3 -o
  SessionType=default`. A first value wins in ssh, so these beat the config's: no shared master carries the forward
  or outlives runnerctl, a prompt fails instead of hanging, `ssh` never forks away from the process runnerctl
  stops, no command of theirs runs, and the log holds the lines read below and no debug lines after them.
  `ForkAfterAuthentication` and `SessionType` need OpenSSH 8.7 or later; an older `ssh` refuses them and the error
  says so. That remote command is killed after 20 s.
- **Failures are worded.** `ClientError::Unreachable { machine, reason }`: ssh's last line, and when it needed a
  person (a refused key, a host key, a passphrase, Tailscale's browser check) that `BatchMode` cannot ask for, the
  advice to run `ssh <target>` once by hand; a changed host key keeps ssh's own words, since signing in does not fix
  it. A request whose channel `ssh` said, since that request began, it could not open reads as "runnerd is not
  running there", with the remote socket; a request that broke because `ssh` exited reads as `ssh` failing, with
  what it said.
- **`machines.toml`.** `$XDG_CONFIG_HOME/runnerd/machines.toml` holds each machine's name (the rule of `[host]
  name`), SSH target, remote socket and machine id. `config::machines` parses, checks and edits it; the CLI reads and
  writes the file. A target starting with `-` or holding a space or control character is refused, in the file too.
- **`runnerctl machine add <name> --ssh <target>`** asks the remote shell for its runtime directory (or takes
  `--remote-socket`), reads the daemon's status once through the forward, and keeps its `machine.id`. It refuses a
  name or id already added, an id equal to this machine's daemon's, an empty id, and a daemon that names no machine
  id (before 0.1.6). It reads the file again just before writing, so a change another runnerctl made meanwhile is
  kept, and writes through a new file of its own process. `machine list` and `machine rm` edit the file.
- **`--machine <name>` on every command**, conflicting with `--socket`. `web --qr` and `web --sign-out-all` use the
  API and work; plain `web` reads a login file beside the socket and is refused, as `tui` without a terminal is,
  before `ssh` starts. A command runnerctl prints to run next (`pause --force`, `cancel` and `rerun --confirm`,
  signing in again) carries the same `--machine`. `doctor` never falls back to this machine's `runnerd doctor` for
  another machine.
- **The spawn is a boundary.** `SshProgram` runs `ssh` (`OpenSsh`); tests give a fake that binds the forward's socket
  itself, and the CLI's end-to-end tests put a fake `ssh` script first on `PATH`.

## Consequences

- No daemon listener, no new dependency, no `runnerctl` needed on the other machine.
- With `--machine`, and in `runnerctl fleet` (ADR 0165), SIGTERM and SIGINT end runnerctl by dropping its
  clients, which stops `ssh` and removes the directories; the TUI does the same through its own handlers, and
  `shell` leaves Ctrl-C to the remote shell. A runnerctl killed with `SIGKILL` leaves its `ssh` and directory
  behind.
- A remote login shell that cannot run `printf` with `${XDG_RUNTIME_DIR:-…}` needs `--remote-socket` at
  `machine add`.
- The fleet view (ADR 0165) and the TUI's switcher (ADR 0166) build on `Endpoint`.
