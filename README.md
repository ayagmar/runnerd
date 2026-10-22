# runnerd

runnerd runs ephemeral GitHub Actions runners on a Linux host: one fresh container per job, from one pool shared by the repositories you configure, kept inside a memory and CPU budget.

It is in early development. Today it checks its config and the host; it does not start runners yet.

## Install

Releases are early 0.0.x builds for Linux on x86_64 and aarch64. The installer downloads `runnerd` and `runnerctl` from the latest release, checks their SHA-256 against the release's checksum files, copies them to `~/.local/bin` and runs `runnerd doctor`:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
```

Pass `--prefix DIR` to install into `DIR/bin` instead, or `--version v0.0.1` for a specific release:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd
```

It never uses sudo and installs nothing else. Every release archive, `install.sh` and `sha256.sum` has a GitHub build provenance attestation. To check `install.sh` before running it, or an archive you downloaded yourself:

```sh
curl -fsSLO https://github.com/ayagmar/runnerd/releases/latest/download/install.sh
gh attestation verify install.sh -R ayagmar/runnerd
sh install.sh
gh attestation verify runnerd-x86_64-unknown-linux-musl.tar.xz -R ayagmar/runnerd
```

## Requirements

- Linux with cgroup v2, and Docker using the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so each runner container can run its own Docker Engine without being privileged. Sysbox publishes `.deb` packages; on Arch Linux it is the AUR package `sysbox-ce-bin`; elsewhere, build it from source.
- A systemd slice whose `MemoryMax` is the budget for all runners.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

`runnerd doctor` checks each of these and prints the fix for what is missing. Given a config, it also checks the slice the config names and that the config fits in it:

```sh
runnerd doctor runnerd.toml
```

## Runner image

[`image/`](image) builds the container every job runs in: the GitHub Actions runner, its own Docker Engine, and one Dockerfile stage per profile on a common base. A profile's `image_stage` picks the stage:

```sh
docker build --target trellis -t runnerd-runner:trellis image
```

Documentation: <https://ayagmar.github.io/runnerd/>

## Config

One file, `runnerd.toml`, lists the repositories to serve, the runner profile each one uses, the shared caches and the systemd slice whose limits form the budget. [`examples/runnerd.toml`](examples/runnerd.toml) is a complete example.

```sh
runnerd check-config runnerd.toml
```

This reads the slice's `MemoryMax` from systemd and fails when `slots × memory` over all profiles, plus the Docker Hub mirror, does not fit in it. A slice without a unit file has no `MemoryMax`; the check passes with a warning, since nothing would hold the runners to their caps. To check against a size without systemd, pass it: `--memory-max 33G`.

Sizes take a binary unit (`K`, `M`, `G`, `T`), as Docker and systemd use them.

`isolation = "unsafe-dind"` runs privileged containers, so a job can take over the host. It is refused unless the `[host]` table also sets `allow_unsafe = true`.

## Building

```sh
cargo build --release
```

The toolchain is pinned in `rust-toolchain.toml`. Before sending a change, run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

[CONTRIBUTING.md](CONTRIBUTING.md) explains which CI jobs a change triggers.

The test that starts a real runner container is ignored by default. It needs Docker with sysbox and the image above:

```sh
cargo test -p runnerd-core --test docker_smoke -- --ignored
```

## License

MIT
