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
docker build --target jvm -t runnerd-runner:jvm image
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

### History

runnerd records every job its runners may run in a SQLite database, `$XDG_STATE_HOME/runnerd/runnerd.db` by default
(`[history]` in the config changes the path and what is kept how long). For each job it keeps:

- its timeline: queued, its runner's container started, the job started, finished, and the container and runner
  registration removed;
- its steps with their start and end, as GitHub shows them;
- CPU, memory and network of its container, sampled every 2 s from the container's cgroup files (memory without page
  cache, as `docker stats` counts it) and `/proc/<pid>/net/dev`; the samples for 7 days, the job's totals and peaks
  for good;
- the bytes the Docker Hub mirror served it, from the mirror's access log;
- its log, up to 50M, as GitHub shows it, with the secrets the runner masked still masked.

It also keeps the sizes of the cache volumes, measured at start and whenever no runner runs after jobs ended, and the
slice's CPU and memory use.

Where the data comes from:

- **The job a runner took** comes from inside its container: runnerd runs a small script there (`docker exec`, as
  root) that reads the job's ID from the runner's `_diag/Worker_*.log` within a second of the job starting. That is
  sooner than GitHub's API, whose list of queued jobs still shows a job for several seconds after a runner took it;
  runnerd does not start a second runner for a job one of its runners holds.
- **Step timing comes from GitHub's jobs API** (`GET /repos/{owner}/{repo}/actions/jobs/{id}`), read every poll while a
  job runs, not from the runner's `_diag` logs. At runner 2.338.1 the Worker log has every step's start and end, but
  under the runner's internal names (`Set up runner`, `Pre Job Hook`), not the ones GitHub shows (`Set up job`,
  `Complete job`), and in a free-form trace that changes between runner versions. The API's steps have second
  precision, which is all GitHub keeps.
- **The log** comes from the runner's `_diag/pages`, which the same script follows. The runner writes the job's log
  there in 4 KiB blocks and deletes each page once uploaded, so lines arrive in blocks of 4 KiB, and a quiet step's
  lines arrive when it ends.

### API

runnerd serves JSON on `$XDG_RUNTIME_DIR/runnerd.sock` (mode 0600; only the user it runs as may connect). The types are
in [`crates/core/src/api.rs`](crates/core/src/api.rs) and its modules. Times are seconds since the Unix epoch, sizes
bytes, CPU millicores.

| Request | Response |
|---|---|
| `GET /v1/status` | profiles and their slots, running runners, repositories with their queued jobs, the memory budget |
| `POST /v1/repositories/{owner}/{name}/pause?force=` | the repository gets no new runners; `409` naming the job while it runs one, unless `force=true`, which cancels the job's run |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/profiles/{name}/reseed` | `202`; the profile starts no runner until its slots are free and slot 1 became its snapshot; `409` while it reseeds |
| `POST /v1/caches/{name}/clean` | `204`; removes a slot's volume or a shared cache; `409` naming the jobs that use it |
| `GET /v1/config` | the config file's path and text |
| `POST /v1/config/check` with TOML | what `runnerd check-config` says about it |
| `PUT /v1/config` with TOML | checks it, writes it in place of the file and runs with it; if it is invalid (`422`), cannot be written (`500`) or runnerd is stopping (`503`), the file and what runs stay as they were |
| `GET /v1/doctor` | what `runnerd doctor` checks, for the config runnerd runs with |
| `POST /v1/shutdown` with `{"force": false}` | `202` |
| `GET /v1/jobs?repository=&state=&conclusion=&since=&until=&before=&limit=` | a page of jobs, newest first; `next` is the `before` of the next page |
| `GET /v1/jobs/{id}` | a job with its timeline, steps, metrics and log size |
| `GET /v1/jobs/{id}/samples` | its CPU, memory and network over time |
| `GET /v1/jobs/{id}/logs?from=&follow=` | its log as text; with `follow=true`, open until the job's record is final |
| `GET /v1/host/samples?since=` | the slice's CPU and memory over time |
| `GET /v1/stats?repository=&since=&until=&bucket=day` | finished jobs per repository and day (or `hour`): counts, queue and run time, CPU, memory, mirror bytes |
| `GET /v1/caches`, `GET /v1/caches/history?since=` | cache volume sizes and their limits, now and over time |
| `GET /v1/events?job=&logs=` | server-sent events: `job.queued`, `job.started`, `job.step`, `job.finished`, `slot.changed`, `budget.changed`, `cache.reseeded`, and live `job.sample`, `host.sample` and (with `logs=true`) `log.line` |

A forced pause cancels the run of each of the repository's running jobs (GitHub cancels runs, not single jobs, so
the run's other jobs are cancelled too) and lets the runner report it cancelled, which takes under a minute; a runner
still running 90 s later is removed. Its runner registration is deleted once GitHub lets go of it.

A new config takes effect without a restart, and running jobs keep running. A removed profile, or slots past a lowered
`slots`, stay until their runners finish, and no runner starts in a profile while it runs as many as its `slots`.
A runner still waiting for a job of a removed repository, or of one moved to another profile, is removed.
Runners started afterwards get the new limits, image and labels. `history.database` changes only at the next start.

Every event but the live ones has an ID; send the last one seen as `Last-Event-ID` and the stream resumes after it,
across daemon restarts. Live events are not stored: a client that missed some reads the job's samples or log.

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/status
curl -N --unix-socket "$XDG_RUNTIME_DIR/runnerd.sock" http://runnerd/v1/events
```

### Dashboard

`runnerd serve` also serves a web dashboard on `127.0.0.1:7477` (`--web <address>` picks another loopback address,
`--web off` serves none). It shows:

- the memory budget as blocks, one per slot sized by its memory cap and filled to the job's last sampled memory;
- every slot with its job, current step, and memory and CPU over the last minutes;
- the slice's memory and CPU over the last half hour, against `MemoryMax` and the CPUs of all slots;
- each job's phases, its steps on one time axis, its CPU and memory against the slot's limits, and its log;
- the history, filtered by repository and outcome, with 30 days of jobs per day, run and wait times and memory peaks;
- the cache volumes with their bounds and their sizes over 30 days;
- pause and resume for each repository.

Open pages update as things happen, over one server-sent event stream per page. Memory and CPU are sampled every 2 s,
so a peak shorter than that does not show; the dashboard calls them sampled peaks.

Any local user can reach the port, so every page needs a session cookie, which a browser gets from a one-time login
URL. The daemon keeps the current URL in `runnerd.web` next to its socket (`$XDG_RUNTIME_DIR/runnerd.web`, mode 0600)
and writes a new one each time one is used. `runnerctl web` opens it; without the CLI:

```sh
xdg-open "$(cat "$XDG_RUNTIME_DIR/runnerd.web")"
```

Writes such as a pause also need an `Origin` of the dashboard itself, so another site the browser has open cannot
send them. The pages load nothing from elsewhere: the Datastar script and the font are in the binary.

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
docker build --target jvm -t runnerd-runner:jvm image
docker build --target base -t runnerd-runner:base image
cargo test -p runnerd-core --test docker_smoke --test cache_reseed -- --ignored
```

[`examples/sandbox.toml`](examples/sandbox.toml) is the setup for the end-to-end test against the throwaway repository `ayagmar/runnerd-sandbox`.

## License

MIT
