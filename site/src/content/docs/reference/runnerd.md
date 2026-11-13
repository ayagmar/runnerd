---
title: runnerd
description: Every `runnerd` command and option but simulate's, as `runnerd --help` prints them.
---

<!-- Written by site/scripts/reference/cli.py from `runnerd --help`. Change the help, then run it. -->

Runs ephemeral GitHub Actions runners in containers within a host budget

The other commands are on [runnerd simulate](../runnerd-simulate/).

## `runnerd init <REPOSITORIES>...`

Write a starter config sized for this host and a systemd user unit, then print the steps left.

Never overwrites a file without --force and never runs anything as root: it prints those steps.

- `<REPOSITORIES>...`: The repositories to serve, as owner/name
- `--slice <SLICE>`: The systemd slice runners run in; its MemoryMax is their budget [default: runnerd.slice]
- `--config <CONFIG>`: Where to write the config [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml]
- `--force`: Replace the config and the unit if they exist
- `--memory-ceiling <SIZE>`: The most memory this host can grow to, such as 24G, for a VM whose hypervisor adds memory as the guest asks for it. The config gets it as memory_ceiling, and the first runner is sized for the host as it can grow

## `runnerd check-config [PATH]`

Validate a config file against the slice's memory budget

- `[PATH]`: The config file [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml]
- `--memory-max <MEMORY_MAX>`: Check against this MemoryMax (such as 33G) instead of reading it from systemd

## `runnerd doctor [CONFIG]`

Check that the host can run runners, and print the fix for what it is missing

- `[CONFIG]`: Also check this config against the slice it names [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml, when it exists]

## `runnerd serve`

Run the daemon: schedule runners for queued jobs and serve the API on a Unix socket.

SIGTERM or SIGINT removes idle runners and exits within 45s, leaving running jobs to the next start, which adopts them; a second one kills them.

- `--config <CONFIG>`: The config file [default: $XDG_CONFIG_HOME/runnerd/runnerd.toml]
- `--socket <SOCKET>`: The API socket [default: $XDG_RUNTIME_DIR/runnerd.sock]
- `--web <WEB>`: Serve the web dashboard on this loopback address; `off` serves none [default: 127.0.0.1:7477]

## `runnerd stop`

Ask the running daemon to stop once its running jobs finish

- `--force`: Kill running jobs and delete their runners instead of waiting
- `--socket <SOCKET>`: The API socket [default: $XDG_RUNTIME_DIR/runnerd.sock]
