---
title: Install
description: Install runnerd, write a starter config and a user unit with runnerd init, and start the service.
---

## What the host needs

- Linux with cgroup v2, and Docker with the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so runner containers run Docker unprivileged: `.deb` packages, the AUR package `sysbox-ce-bin` on Arch Linux, or a source build.
- A systemd slice whose `MemoryMax` is the budget for all runners. `runnerd init` prints one sized for the host.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

The binaries are built for x86_64 and aarch64. The runner images are linux/amd64 only, so on aarch64 runners do not start yet.

## First run

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
runnerd init halyard/ledger halyard/storefront
```

The installer checks the SHA-256 of `runnerd` and `runnerctl`, copies them to `~/.local/bin` and runs `runnerd doctor`. It never uses sudo.

`init` writes two files, runs the doctor against them and prints what is left. On a 62G, 32-CPU host with no slice yet:

```text
$ runnerd init halyard/ledger halyard/storefront
wrote ~/.config/runnerd/runnerd.toml: 5 slots of 8G and 6 CPUs in runnerd.slice (46.5G)
wrote ~/.config/systemd/user/runnerd.service

runnerd doctor:
ok    Docker: reachable, Docker 29.9.2
ok    cgroups: cgroup v2 with the systemd driver
info  isolation: sysbox: runners run unprivileged
ok    sysbox: Docker has the sysbox-runc runtime and sysbox's services run
FAIL  slice: runnerd.slice has no unit file with a MemoryMax, so nothing holds runners to the budget
      fix: sudo tee /etc/systemd/system/runnerd.slice >/dev/null <<'EOF'
           [Slice]
           MemoryMax=46.5G
           EOF
           sudo systemctl daemon-reload
ok    config: valid, with warnings: the slice runnerd.slice has no MemoryMax, so nothing holds runners to their 40.5G of caps; install a unit file for it
ok    image: ghcr.io/ayagmar/runnerd-runner:base-0.0.5: not pulled yet; runnerd pulls it before the first runner
ok    gh: logged in to github.com with the repo scope
info  /dev/kvm: present, for a future Kata Containers backend

First run the fixes above for slice; runnerd init runs nothing as root.
Start runnerd and have it start at login; it pulls ghcr.io/ayagmar/runnerd-runner:base-0.0.5 before the first runner:
  systemctl --user daemon-reload
  systemctl --user enable --now runnerd
  runnerctl status
To keep it running while you are logged out: loginctl enable-linger "$USER"
```

Run the root commands your `init` printed, then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now runnerd
runnerctl status
```

| Flag | |
|---|---|
| `--slice NAME` | another slice than `runnerd.slice` |
| `--config PATH` | write the config elsewhere |
| `--force` | replace files that exist; without it `init` writes neither |

The profile gets three quarters of the host's memory, or the slice's `MemoryMax` if smaller. [Configuration](../configuration/) says what to change.

## Pick a directory or release

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd --version v0.0.5
```

From crates.io, `runnerctl` is in `runnerd-cli`:

```sh
cargo install --locked runnerd runnerd-cli   # Rust 1.99 or later
cargo binstall runnerd runnerd-cli           # the release archives
```

## Verify a release

Every release archive, `install.sh` and `sha256.sum` has a GitHub build provenance attestation, and so does every runner image:

```sh
curl -fsSLO https://github.com/ayagmar/runnerd/releases/latest/download/install.sh
gh attestation verify install.sh -R ayagmar/runnerd
sh install.sh
gh attestation verify oci://ghcr.io/ayagmar/runnerd-runner:base-0.0.5 --repo ayagmar/runnerd
```
