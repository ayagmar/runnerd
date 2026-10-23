# runnerd

runnerd runs ephemeral GitHub Actions runners on a Linux host: one fresh container per job, from one pool shared by the repositories you configure, kept inside a memory and CPU budget.

It is in early development: the daemon schedules runners, records every job, can keep a failed job's runner to open a shell in, serves a JSON API and a web dashboard, and `runnerctl` reads and controls it from a terminal.

## First run

On a Linux host with Docker:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
runnerd init halyard/ledger halyard/storefront
```

Serve only private repositories: on a public one, anyone who opens a pull request can run code on this host
([Configuration](https://ayagmar.github.io/runnerd/configuration/)).

`runnerd init` writes two files and changes nothing else:

- `~/.config/runnerd/runnerd.toml`: the repositories, and one profile with slots of up to 8G, one per 4 CPUs, inside
  three quarters of the host's memory (or the slice's `MemoryMax` if that is smaller), so `runnerd check-config`
  passes;
- `~/.config/systemd/user/runnerd.service`: a user unit running `runnerd serve` with that config.

It then runs `runnerd doctor` and prints what is left as commands. It prints the ones that need root and never runs
them; on a fresh host they are usually the slice, sized for this host, and Sysbox. Run the commands your `init`
printed, not a copy from elsewhere.

Then start the service. The config's profile runs this release's published image,
`ghcr.io/ayagmar/runnerd-runner:base-<version>`, which runnerd pulls before the first runner. Published images are
linux/amd64 only today, so on aarch64 runners do not start yet.

```sh
systemctl --user daemon-reload
systemctl --user enable --now runnerd
runnerctl status
```

`init` never replaces a file unless given `--force`. `--slice` names another slice, `--config` writes the config
elsewhere.

## Install

The installer downloads `runnerd` and `runnerctl` for Linux x86_64 or aarch64, checks their SHA-256, copies them to
`~/.local/bin` and runs `runnerd doctor`. It never uses sudo. To pick the directory or the release:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd --version v0.0.6
```

For v0.0.3 and earlier, run that release's own `install.sh`. Every release archive, `install.sh` and `sha256.sum` has
a GitHub build provenance attestation:

```sh
curl -fsSLO https://github.com/ayagmar/runnerd/releases/latest/download/install.sh
gh attestation verify install.sh -R ayagmar/runnerd
sh install.sh
```

From crates.io the daemon is `runnerd` and `runnerctl` is in `runnerd-cli`. `cargo install` needs Rust 1.99 or later;
`cargo binstall` downloads the release archives instead:

```sh
cargo install --locked runnerd runnerd-cli
cargo binstall runnerd runnerd-cli
```

## Requirements

- Linux with cgroup v2, and Docker using the systemd cgroup driver.
- [Sysbox](https://github.com/nestybox/sysbox), so each runner container can run its own Docker Engine without being
  privileged.
- A systemd slice whose `MemoryMax` is the budget for all runners; `runnerd init` prints one sized for the host.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

`runnerd doctor` checks each of these and prints the fix for what is missing.

## Documentation

The site, <https://ayagmar.github.io/runnerd/>, has the guides:

- [Install](https://ayagmar.github.io/runnerd/install/): requirements, `runnerd init`'s output, verifying a release.
- [Configuration](https://ayagmar.github.io/runnerd/configuration/): `runnerd.toml`, profiles, runner images, records of failed jobs, the budget check, isolation.
- [Running](https://ayagmar.github.io/runnerd/running/): the service unit and `systemctl`, how runners are scheduled and caches kept warm, records of failed jobs and their shells, the job history, and the JSON API and its events.
- [Dashboard and CLI](https://ayagmar.github.io/runnerd/clients/): the web dashboard, every `runnerctl` command, and `runnerctl tui` with its keys.

[ARCHITECTURE.md](https://github.com/ayagmar/runnerd/blob/main/ARCHITECTURE.md) says where the code lives and [docs/adr](https://github.com/ayagmar/runnerd/blob/main/docs/adr/README.md) why.

## Building

```sh
cargo build --release
```

The toolchain is pinned in `rust-toolchain.toml`. Before sending a change, run the checks listed under "The gate" in
[AGENTS.md](https://github.com/ayagmar/runnerd/blob/main/AGENTS.md); CI runs the same. [CONTRIBUTING.md](https://github.com/ayagmar/runnerd/blob/main/CONTRIBUTING.md) explains which CI jobs a change
triggers and how a release is cut.

The tests that start real containers are ignored by default. They need Docker with sysbox and the images:

```sh
docker build --target jvm -t runnerd-runner:jvm image
docker build --target base -t runnerd-runner:base image
cargo test -p runnerd-core --test docker_smoke --test cache_reseed -- --ignored
```

## License

MIT
