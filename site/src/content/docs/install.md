---
title: Install
description: Install runnerd, write a starter config and a user unit with runnerd init, and start the service.
---

## Requirements

- Linux with cgroup v2, and Docker with the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so runner containers run Docker unprivileged. `runnerd doctor` prints the install for your distribution.
- A systemd slice whose `MemoryMax` is the budget for all runners. `runnerd init` prints one sized for the host.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

Binaries ship for x86_64 and aarch64; the runner images are linux/amd64 only, so on aarch64 build the `base` stage of `image/Dockerfile` on the host and name it with `image_stage` (`runnerd doctor` prints the command).

`runnerd doctor` checks each requirement, the slice the config names, that the config fits in it and who can read the config, and warns for each public repository the config serves. It reads `~/.config/runnerd/runnerd.toml`; `runnerd doctor <file>` checks another.

## First run

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
runnerd init halyard/ledger halyard/storefront
```

The installer checks the SHA-256 of `runnerd` and `runnerctl`, copies them to `~/.local/bin` and runs `runnerd doctor`. It never uses sudo. `init` writes `~/.config/runnerd/runnerd.toml` and `~/.config/systemd/user/runnerd.service`, runs the doctor against them and prints what is left. On a 62G, 32-CPU host with no slice yet (checks shortened):

```text
$ runnerd init halyard/ledger halyard/storefront
wrote ~/.config/runnerd/runnerd.toml: 5 slots of 8G and 6 CPUs in runnerd.slice (46.5G)
wrote ~/.config/systemd/user/runnerd.service

runnerd doctor:
ok    Docker: reachable, Docker 29.9.2
info  isolation: sysbox: runners run unprivileged
FAIL  slice: runnerd.slice has no unit file with a MemoryMax, so nothing holds runners to the budget
      fix: sudo tee /etc/systemd/system/runnerd.slice >/dev/null <<'EOF'
           [Slice]
           MemoryMax=46.5G
           EOF
           sudo systemctl daemon-reload
ok    gh: logged in to github.com with the repo scope
ok    repository: halyard/ledger: private
[...]

First run the fixes above for slice; runnerd init runs nothing as root.
Start runnerd and have it start at login; it pulls ghcr.io/ayagmar/runnerd-runner:base-0.0.26 before the first runner:
  systemctl --user daemon-reload
  systemctl --user enable --now runnerd
  runnerctl status
To keep it running while you are logged out: loginctl enable-linger "$USER"

Send a workflow job here with:
  runs-on: [self-hosted, linux, x64]
A job runs here only when every label in its runs-on is in its repository's labels in runnerd.toml; other jobs, such as runs-on: ubuntu-latest, stay on GitHub's runners.
```

Run the root commands `init` printed, then the three `systemctl` and `runnerctl` lines. A public repository gets a warning with its fix:

```text
WARN  repository: halyard/docs-site is public: anyone who opens a pull request on it can run code on this host, and their jobs share runnerd's caches with your trusted jobs
      fix: serve private repositories only; for a public one, have GitHub require approval before it runs workflows from fork pull requests (the repository's Actions settings)
```

| Flag | |
|---|---|
| `--slice NAME` | another slice than `runnerd.slice` |
| `--config PATH` | write the config elsewhere |
| `--force` | replace files that exist; without it `init` writes neither |
| `--memory-ceiling SIZE` | for a VM whose memory grows on demand, such as Hyper-V Dynamic Memory: the most it can grow to, written as `memory_ceiling` ([A host whose memory grows](../admission/#a-host-whose-memory-grows)) |

`init` sizes slots of up to 8G, one per 4 CPUs, inside three quarters of the host's memory or the slice's `MemoryMax` if smaller. A slot is also no larger than the host has available now, less the mirror's 512M, [the host's margin](../admission/#the-hosts-memory) and 512M for what starts with runnerd. On a host that leaves less than 2G for a runner, such as a 4G VM, `init` writes nothing and says why. [Configuration](../configuration/) says what to change.

## Directory and version

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd --version v0.0.26
```

From crates.io, `runnerctl` is in `runnerd-cli`:

```sh
cargo install --locked runnerd runnerd-cli   # Rust 1.99 or later
cargo binstall runnerd runnerd-cli           # the release archives
```

## Verify a release

Every release archive, `install.sh`, `sha256.sum` and runner image has a GitHub build provenance attestation:

```sh
curl -fsSLO https://github.com/ayagmar/runnerd/releases/latest/download/install.sh
gh attestation verify install.sh -R ayagmar/runnerd
sh install.sh
gh attestation verify oci://ghcr.io/ayagmar/runnerd-runner:base-0.0.26 --repo ayagmar/runnerd
```
