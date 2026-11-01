# runnerd

[![CI](https://github.com/ayagmar/runnerd/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/ayagmar/runnerd/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ayagmar/runnerd)](https://github.com/ayagmar/runnerd/releases/latest)
[![crates.io](https://img.shields.io/crates/v/runnerd)](https://crates.io/crates/runnerd)
[![MSRV](https://img.shields.io/crates/msrv/runnerd)](rust-toolchain.toml)
[![License: MIT](https://img.shields.io/github/license/ayagmar/runnerd)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-ayagmar.github.io%2Frunnerd-1f6b3a)](https://ayagmar.github.io/runnerd/)

runnerd runs ephemeral GitHub Actions runners on one Linux host: a fresh container per job, sized from what the repositories you serve have used.

## Install

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
runnerd init halyard/ledger halyard/storefront
```

`init` writes a config sized for the host and a systemd user unit, runs `runnerd doctor` and prints the steps left. It never runs anything as root; the slice and Sysbox commands it prints are yours to run. Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now runnerd
runnerctl status
```

Point a job at the host with `runs-on: [self-hosted, linux, x64]`. Serve private repositories only: on a public one, anyone who opens a pull request can run code on this host.

## Requirements

- Linux with cgroup v2, and Docker with the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox) and a systemd slice whose `MemoryMax` is the runners' budget.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

Binaries ship for x86_64 and aarch64; the runner images are linux/amd64 only, so on aarch64 build the `base` stage of `image/Dockerfile` on the host.

## Documentation

Full docs: <https://ayagmar.github.io/runnerd/>. [Install](https://ayagmar.github.io/runnerd/install/) covers pinned versions and attestation checks, [Configuration](https://ayagmar.github.io/runnerd/configuration/) every key, and [Dashboard and CLI](https://ayagmar.github.io/runnerd/clients/) what you can watch and do.
[ARCHITECTURE.md](https://github.com/ayagmar/runnerd/blob/main/ARCHITECTURE.md) says where the code lives, [docs/adr](https://github.com/ayagmar/runnerd/blob/main/docs/adr/README.md) why, and [CONTRIBUTING.md](https://github.com/ayagmar/runnerd/blob/main/CONTRIBUTING.md) how to send a change.

## Building

```sh
cargo build --release
```

Run "The gate" in [AGENTS.md](https://github.com/ayagmar/runnerd/blob/main/AGENTS.md) before sending a change. Tests that start real containers are ignored by default and need Docker with Sysbox and the images:

```sh
docker build --target jvm -t runnerd-runner:jvm image
docker build --target base -t runnerd-runner:base image
cargo test -p runnerd-core --test docker_smoke --test cache_reseed --test slot_docker_store -- --ignored
```

The Gradle build cache test also needs the network:

```sh
cargo test -p runnerd-core --test build_cache_gradle -- --ignored
docker rmi runnerd-test-build-cache
```

## License

MIT
