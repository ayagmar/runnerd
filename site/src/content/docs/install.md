---
title: Install
description: Install runnerd and runnerctl on a Linux host and check that the host can run runners.
---

runnerd is in early development: the daemon schedules runners and serves a small JSON API; job history, metrics, the CLI client and the web dashboard come next. Releases are early 0.0.x builds for Linux on x86_64 and aarch64.

## What the host needs

- Linux with cgroup v2, and Docker using the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so each runner container can run its own Docker Engine without being privileged. Sysbox publishes `.deb` packages; on Arch Linux it is the AUR package `sysbox-ce-bin`; elsewhere, build it from source.
- A systemd slice whose `MemoryMax` is the budget for all runners.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

You do not need to set these up first: `runnerd doctor` checks each one and prints the fix for what is missing.

## Install

The installer downloads `runnerd` and `runnerctl` from the latest release, checks their SHA-256 against the release's checksum files, copies them to `~/.local/bin` and runs `runnerd doctor`:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
```

Pass `--prefix DIR` to install into `DIR/bin` instead, or `--version v0.0.1` for a specific release:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd
```

It never uses sudo and installs nothing else.

## Verify a release

Every release archive, `install.sh` and `sha256.sum` has a GitHub build provenance attestation. To check `install.sh` before running it, or an archive you downloaded yourself:

```sh
curl -fsSLO https://github.com/ayagmar/runnerd/releases/latest/download/install.sh
gh attestation verify install.sh -R ayagmar/runnerd
sh install.sh
gh attestation verify runnerd-x86_64-unknown-linux-musl.tar.xz -R ayagmar/runnerd
```

## Check the host

`runnerd doctor` runs its checks in the order a new machine needs them. Given a config, it also checks the slice the config names and that the config fits in it:

```sh
runnerd doctor runnerd.toml
```

On a host that is ready:

```text
ok    Docker: reachable, Docker 29.9.2
ok    cgroups: cgroup v2 with the systemd driver
info  isolation: sysbox: runners run unprivileged
ok    sysbox: Docker has the sysbox-runc runtime and sysbox's services run
ok    slice: ci.slice (/etc/systemd/system/ci.slice): MemoryMax=33G, CPU quota infinity
ok    config: fits in ci.slice
ok    gh: logged in to github.com with the repo scope
info  /dev/kvm: present, for a future Kata Containers backend
```

A failed check prints its fix under it. Here the config names a slice that has no unit file yet:

```text
FAIL  slice: demo.slice has no unit file with a MemoryMax, so nothing holds runners to the budget
      fix: create /etc/systemd/system/demo.slice with
           [Slice]
           MemoryMax=32.5G
           then: sudo systemctl daemon-reload
```

`doctor` exits non-zero while any check fails. Next, write a [config](../configuration/), then [run the daemon](../running/).
