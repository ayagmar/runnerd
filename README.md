# runnerd

runnerd runs ephemeral GitHub Actions runners on a Linux host: one fresh container per job, from one pool shared by the repositories you configure, kept inside a memory and CPU budget.

It is in early development: the daemon schedules runners and serves a small JSON API; job history, metrics, the CLI client and the web dashboard come next.

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

`runnerd` runs the image `runnerd-runner:<image_stage>`; build the stages your profiles use before starting it.

Documentation: <https://ayagmar.github.io/runnerd/>

## Running

```sh
runnerd serve --config runnerd.toml
```

Without `--config`, it reads `$XDG_CONFIG_HOME/runnerd/runnerd.toml`. It reads the `gh` CLI's token at start (and again when GitHub rejects it), then every `poll_interval` (`[host]`, default `15s`) asks GitHub for each repository's queued jobs. A job is served when every label in its `runs-on` is one of its repository's `labels`. For each such job it registers a just-in-time runner on the repository and starts it in a free slot of the repository's profile, if the slice's `MemoryMax` still has room for the profile's `memory`. The runner takes one job, its container exits, and runnerd removes it; a runner that got no job within 5 minutes is removed. Jobs that cannot be served yet wait in GitHub's queue.

A slot is one concurrent job of a profile. Containers are new for every job, but each slot keeps a volume with its Docker Engine's images and its own changes to `~/.cache`, the tool cache and the Android SDK, over the profile's read-only snapshot. When a profile has no job running and its slot 1 changed those by `reseed_at` (`[caches]`, default `2G`), slot 1's changes become the new snapshot. The pnpm store and the Maven repository are shared by all slots and emptied above their `max_size` when no job runs. The Docker Hub mirror runs while any runner does.

A restarted runnerd adopts the runner containers still running and deletes this host's runners that have no container. SIGTERM or SIGINT, or `runnerd stop`, stops polling and waits for running jobs; a second signal, or `runnerd stop --force`, kills them.

### API

runnerd serves JSON on `$XDG_RUNTIME_DIR/runnerd.sock` (mode 0600; only the user it runs as may connect). The types are in `crates/core/src/api.rs`.

| Request | Response |
|---|---|
| `GET /v1/status` | profiles and their slots, running runners, repositories with their queued jobs, the memory budget |
| `POST /v1/repositories/{owner}/{name}/pause` | `204`; the repository gets no new runners, running jobs finish |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/shutdown` with `{"force": false}` | `202` |

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/status
```

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

The toolchain is pinned in `rust-toolchain.toml`. Before sending a change, run the checks listed under "The gate" in
[AGENTS.md](AGENTS.md); CI runs the same. [CONTRIBUTING.md](CONTRIBUTING.md) explains which CI jobs a change triggers,
and [ARCHITECTURE.md](ARCHITECTURE.md) where the code lives.

The tests that start real containers are ignored by default. They need Docker with sysbox and the images:

```sh
docker build --target trellis -t runnerd-runner:trellis image
docker build --target base -t runnerd-runner:base image
cargo test -p runnerd-core --test docker_smoke --test cache_reseed -- --ignored
```

[`examples/sandbox.toml`](examples/sandbox.toml) is the setup for the end-to-end test against the throwaway repository `ayagmar/runnerd-sandbox`.

## License

MIT
