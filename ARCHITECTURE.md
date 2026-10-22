# Architecture

Where things live and which rules hold them together. The why behind each rule is in [`docs/adr`](docs/adr/README.md).

## Crates

| Crate | Path | Owns | May depend on |
|---|---|---|---|
| `runnerd-core` | `crates/core` | config, API types, scheduling, Docker and GitHub clients, caches, job history | anything; Docker, GitHub and Tokio only behind the `host` feature |
| `runnerd` | `crates/daemon` | the daemon binary: `serve`, `stop`, `doctor`, `check-config`, the Unix-socket API | `runnerd-core` with `host` |
| `runnerctl` | `crates/cli` | the command-line client | `runnerd-core` without `host` |

`runnerd-core` modules:

- `api`, `config`: build without `host`. Every client reads these.
- `docker`, `github`, `cache`, `isolation`, `scheduler`, `observe`, `store`: `host` only ([`crates/core/src/lib.rs`](crates/core/src/lib.rs)).

The web dashboard (`crates/web`) and the desktop app (`crates/desktop`) will be clients like `runnerctl`.
[`.github/scripts/check-boundaries.sh`](.github/scripts/check-boundaries.sh) fails CI when any workspace crate other
than `runnerd` and `runnerd-core` builds `bollard`, `reqwest` or core's `host` feature.

## Request path

client → transport → principal → handler → core

1. A client sends JSON over the daemon's Unix socket (`$XDG_RUNTIME_DIR/runnerd.sock`).
2. The transport ([`crates/daemon/src/api.rs`](crates/daemon/src/api.rs)) reads the peer's credentials and turns
   them into an `api::Principal` in one extractor, `Authenticated`. Only the user the daemon runs as gets through.
3. Handlers take that principal, read `Handle::status` or the job store (`Handle::store`), or call `Control`
   (pause, resume, shutdown). `GET /v1/events` streams the store's events as server-sent events.
4. The engine ([`crates/core/src/scheduler/engine.rs`](crates/core/src/scheduler/engine.rs)) owns all slot state.
   It publishes `api::Status` after every tick and takes commands only through `Control`.

A new transport (SSH, Tailscale) produces a `Principal` and reuses the handlers.

## Job lifecycle

queued → slot → container → removed

1. Every `poll_interval` the engine asks GitHub for runners first, then queued jobs (`Engine::tick`; the order
   avoids starting a second runner for a job its runner just took).
2. `scheduler::plan`, a pure function, picks a free slot per queued job within the memory budget.
3. `Engine::start` prepares the slot's volumes, registers a JIT runner (`GithubApi::jit_config`) and starts its
   container (`DockerApi::start`) under the profile's limits and isolation backend.
4. The runner takes one job and exits. `Engine::collect_finished` sees the container stopped, removes it and the
   runner's registration, and frees the slot. A runner that took no job holds the repository back for a while.
5. While a job runs the engine records it in the store: the job's ID from the runner's `_diag` Worker log
   (`observe::capture`, run in the container with `docker exec`), its steps from `GithubApi::job`, its log from the
   runner's `_diag/pages`, CPU and memory from the container's cgroup every 2 s (`observe::metrics`), and the
   mirror's bytes from its access log (`observe::mirror`). The record is final once GitHub reports the job completed
   and the container is gone, or 15 minutes after the container is gone (`lost`).
6. When a profile is idle after its slot 1 ran a job, `cache::reseed` makes slot 1's layers the profile's snapshot.

On start the engine adopts the runner and seed containers a previous daemon left (`Engine::recover`) and schedules
nothing until Docker has listed them.

## State

| What | Where |
|---|---|
| Config | `runnerd.toml`, default `$XDG_CONFIG_HOME/runnerd/runnerd.toml` ([`crates/core/src/config.rs`](crates/core/src/config.rs)) |
| Slot and repository state | in memory, in `Engine`; rebuilt from container labels on restart |
| Caches | Docker volumes named by `cache::Names`: `runnerd-<profile>-base`, `runnerd-<profile>-slot-<n>`, `runnerd-pnpm`, `runnerd-maven`, `runnerd-mirror` |
| Runner containers | labelled `runnerd.job`, `runnerd.profile`, `runnerd.slot`, `runnerd.repository`, `runnerd.runner-id` |
| Budget | the systemd slice's `MemoryMax`, read with `systemctl` |

| Job history | SQLite, default `$XDG_STATE_HOME/runnerd/runnerd.db` (`store::Store`); jobs, runners, steps, samples, logs, events, cache sizes; times in ms |

## Invariants

- API types live only in `runnerd-core::api`. Clients deserialize the same types the daemon serializes.
- Clients never talk to Docker or GitHub; they talk to the daemon.
- The GitHub token stays inside `github::Github`: never in a log line, a `Debug` output or an error.
- A JIT config reaches a container only as a root-only file (`docker::JIT_CONFIG_PATH`), never the create request
  or a log line.
- The engine deletes only GitHub runner registrations whose names start with this machine's prefix
  (`HostIdentity::prefix`); other machines serve the same repositories. Runner containers on the local Docker are
  this machine's whatever their name: `Engine::recover` adopts or removes them without checking the prefix.
- `scheduler::plan` does no I/O and reads no clock.
- Only the store's writer thread writes the database; every write goes through `Store`'s queue, in order.
- A durable event (every kind but `job.sample`, `host.sample` and `log.line`) is stored before it is broadcast, so
  its ID is a position a client can resume from with `Last-Event-ID`.
- The engine does not start a runner for a queued job one of its runners holds, whatever GitHub's queue says.
