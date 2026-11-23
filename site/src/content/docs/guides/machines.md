---
title: How do I read another machine's runnerd?
description: Add the machine once with runnerctl machine add, then pass --machine to any command; runnerctl forwards the daemon's socket over SSH.
---

When a second machine runs its own daemon, `runnerctl` on this one reads it over SSH. Nothing listens on the network: `runnerctl` starts `ssh -N -L` for the length of one command and forwards a private socket to the other daemon's socket. The other machine needs runnerd 0.1.6 or later and your SSH key; it does not need `runnerctl`.

Add it once, by a name of lower-case letters and digits:

```sh
runnerctl machine add skiff --ssh me@skiff.tail0b5e.ts.net
```

`machine add` asks the other machine for its runtime directory, reads its daemon's status, and keeps the SSH target, the socket path and the machine id in `~/.config/runnerd/machines.toml`. It refuses a machine whose id is this machine's or one already added: two daemons with the same id take each other's runners for their own. If the remote shell cannot say where its runtime directory is, pass `--remote-socket /run/user/1000/runnerd.sock`.

Then pass `--machine` to any command:

```sh
runnerctl --machine skiff status
runnerctl --machine skiff tui
runnerctl --machine skiff logs 9001 --follow
```

`runnerctl machine list` shows the machines, `runnerctl machine rm skiff` forgets one.

## Every machine at once

`runnerctl fleet` reads this machine's daemon and every added one together. It prints a line per machine (running, queued, slots held, version) and one page of all their jobs, newest first, with the machine whose record shows. A job both machines saw shows once, from the machine that ran it, not as `elsewhere` from the other. A machine that cannot be read gets a line saying why, and `fleet` exits 1. `--before` and `--limit` page as `runnerctl jobs` does, and `--json` prints the merged view.

## When it fails

`ssh` runs with `BatchMode=yes`, so it cannot ask for a password, a key's passphrase, a new host key or Tailscale SSH's browser check. When one of those is needed, the error says so: run `ssh me@skiff.tail0b5e.ts.net` once yourself, then try again. Your `~/.ssh/config` still says how to reach the host (aliases, user, keys, port, jump host), but not how runnerctl's `ssh` behaves: its `ControlMaster`, `ForkAfterAuthentication`, `RemoteCommand`, `LocalCommand` and `LogLevel` are not used. runnerctl needs OpenSSH 8.7 or later.

`runnerctl --machine skiff web` cannot open the dashboard: its login link is a file on `skiff`. `runnerctl --machine skiff web --qr` prints a fresh link to scan, and `--sign-out-all` works too. `runnerctl --machine skiff doctor` reports a daemon that is down instead of running `runnerd doctor` on this machine.
