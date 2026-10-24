---
title: Install
description: Install runnerd, write a starter config and a user unit with runnerd init, and start the service.
---

## What the host needs

- Linux with cgroup v2, and Docker with the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so runner containers run Docker unprivileged. When it is missing, `runnerd doctor` prints the install for this distribution: Nestybox's `.deb` on Ubuntu and Debian, the AUR package `sysbox-ce-bin` on Arch, a source build on Fedora, CentOS Stream, Rocky, Alma and Amazon Linux, and Nestybox's support table elsewhere. Nestybox supports Red Hat Enterprise Linux with Sysbox-EE only.
- A systemd slice whose `MemoryMax` is the budget for all runners. `runnerd init` prints one sized for the host.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

`runnerd doctor` checks each of these, the slice the config names and that the config fits in it, and prints the fix for what is missing. It reads `~/.config/runnerd/runnerd.toml` when it exists; `runnerd doctor <file>` checks another.

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
ok    image: ghcr.io/ayagmar/runnerd-runner:base-0.0.8: not pulled yet; runnerd pulls it before the first runner
info  records: the records of failed jobs take 0B of [records] max_total = 20G
ok    gh: logged in to github.com with the repo scope
info  /dev/kvm: present, for a future Kata Containers backend

First run the fixes above for slice; runnerd init runs nothing as root.
Start runnerd and have it start at login; it pulls ghcr.io/ayagmar/runnerd-runner:base-0.0.8 before the first runner:
  systemctl --user daemon-reload
  systemctl --user enable --now runnerd
  runnerctl status
To keep it running while you are logged out: loginctl enable-linger "$USER"

Send a workflow job here with:
  runs-on: [self-hosted, linux, x64]
A job runs here only when every label in its runs-on is in its repository's labels in runnerd.toml; other jobs, such as runs-on: ubuntu-latest, stay on GitHub's runners.
```

Run the root commands your `init` printed, then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now runnerd
runnerctl status
```

Then point a job at the runners. `init` gives each repository the labels GitHub gives its own self-hosted runners, so a workflow written for those runs here unchanged:

```yaml
jobs:
  build:
    runs-on: [self-hosted, linux, x64]
```

| Flag | |
|---|---|
| `--slice NAME` | another slice than `runnerd.slice` |
| `--config PATH` | write the config elsewhere |
| `--force` | replace files that exist; without it `init` writes neither |

`init` sizes slots of up to 8G, one per 4 CPUs, inside three quarters of the host's memory, or the slice's `MemoryMax` if smaller. [Configuration](../configuration/) says what to change.

## Pick a directory or release

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd --version v0.0.8
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
gh attestation verify oci://ghcr.io/ayagmar/runnerd-runner:base-0.0.8 --repo ayagmar/runnerd
```
