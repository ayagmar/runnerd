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

- **One managed forward per client.** `Client::open(Endpoint::Ssh(machine), …)` starts `ssh -N -T -o BatchMode=yes
  -o ControlPath=none -o ControlMaster=no -o ConnectTimeout=10 -o StreamLocalBindUnlink=yes -o
  ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -L <dir>/runnerd.sock:<remote> --
  <target>` and waits up to 20 s for the local socket. `<dir>` is a new 0700 directory under `$XDG_RUNTIME_DIR`
  (else the temporary directory), never reused. Clones share the forward; the last one dropped kills `ssh` and
  removes the directory. An `ssh` that exited, as after a network drop, is started again on the next request.
- **The owner's SSH config does not change the child.** `ControlPath=none` keeps a shared master from carrying the
  forward or outliving runnerctl; `BatchMode=yes` makes a prompt fail instead of hang.
- **Failures are worded.** `ClientError::Unreachable { machine, reason }`: ssh's last line, and when it needed a
  person (a refused key, a host key, a passphrase, Tailscale's browser check) that `BatchMode` cannot ask for, the
  advice to run `ssh <target>` once by hand. A request whose channel `ssh` could not open reads as "runnerd is not
  running there", with the remote socket.
- **`machines.toml`.** `$XDG_CONFIG_HOME/runnerd/machines.toml` holds each machine's name (the rule of `[host]
  name`), SSH target, remote socket and machine id. `config::machines` parses, checks and edits it; the CLI reads and
  writes the file. A target starting with `-` or holding a space or control character is refused, in the file too.
- **`runnerctl machine add <name> --ssh <target>`** asks the remote shell for its runtime directory (or takes
  `--remote-socket`), reads the daemon's status once through the forward, and keeps its `machine.id`. It refuses a
  name or id already added, an id equal to this machine's daemon's, and a daemon that names no machine id (before
  0.1.6). `machine list` and `machine rm` edit the file.
- **`--machine <name>` on every command**, conflicting with `--socket`. `web --qr` and `web --sign-out-all` use the
  API and work; plain `web` reads a login file beside the socket and is refused with what works instead. `doctor`
  never falls back to this machine's `runnerd doctor` for another machine.
- **The spawn is a boundary.** `SshProgram` runs `ssh` (`OpenSsh`); tests give a fake that binds the forward's socket
  itself, and the CLI's end-to-end tests put a fake `ssh` script first on `PATH`.

## Consequences

- No daemon listener, no new dependency, no `runnerctl` needed on the other machine.
- A runnerctl killed with `SIGKILL` leaves its `ssh` and directory behind; Ctrl-C reaches `ssh` too, since it runs in
  runnerctl's process group.
- A remote login shell that cannot run `printf` with `${XDG_RUNTIME_DIR:-…}` needs `--remote-socket` at
  `machine add`.
- The fleet view (ADR 0165) and the TUI's switcher (ADR 0166) build on `Endpoint`.
