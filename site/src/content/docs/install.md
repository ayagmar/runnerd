---
title: Install
description: Install runnerd and runnerctl on a Linux host and check that the host can run runners.
---

runnerd is early: 0.0.1 schedules runners and serves a small JSON API. Main adds job history, metrics, `runnerctl`'s commands and the [web dashboard](../clients/), for the next release. Releases are Linux builds for x86_64 and aarch64.

## What the host needs

- Linux with cgroup v2, and Docker with the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so runner containers run Docker unprivileged: `.deb` packages, the AUR package `sysbox-ce-bin` on Arch Linux, or a source build.
- A systemd slice whose `MemoryMax` is the budget for all runners.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

`runnerd doctor` checks each of these and prints the fix for what is missing.

## Install

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
```

It downloads `runnerd` and `runnerctl` from the latest release, checks their SHA-256, copies them to `~/.local/bin` and runs `runnerd doctor`. It never uses sudo and installs nothing else. `--prefix DIR` installs into `DIR/bin`, `--version v0.0.1` picks a release:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd
```

## Verify a release

Every release archive, `install.sh` and `sha256.sum` has a GitHub build provenance attestation:

```sh
curl -fsSLO https://github.com/ayagmar/runnerd/releases/latest/download/install.sh
gh attestation verify install.sh -R ayagmar/runnerd
sh install.sh
gh attestation verify runnerd-x86_64-unknown-linux-musl.tar.xz -R ayagmar/runnerd
```

## Check the host

Given a config, `doctor` also checks the slice it names and that the config fits:

```text
$ runnerd doctor runnerd.toml
ok    Docker: reachable, Docker 29.9.2
ok    cgroups: cgroup v2 with the systemd driver
info  isolation: sysbox: runners run unprivileged
ok    sysbox: Docker has the sysbox-runc runtime and sysbox's services run
ok    slice: ci.slice (/etc/systemd/system/ci.slice): MemoryMax=33G, CPU quota infinity
ok    config: fits in ci.slice
ok    gh: logged in to github.com with the repo scope
info  /dev/kvm: present, for a future Kata Containers backend
```

A failed check prints its fix under it, and `doctor` exits non-zero. Here the config names a slice with no unit file:

```text
FAIL  slice: demo.slice has no unit file with a MemoryMax, so nothing holds runners to the budget
      fix: create /etc/systemd/system/demo.slice with
           [Slice]
           MemoryMax=32.5G
           then: sudo systemctl daemon-reload
```

Next: write a [config](../configuration/), then [run the daemon](../running/).
