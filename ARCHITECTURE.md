# Architecture

Where things live and which rules hold them together. The why behind each rule is in [`docs/adr`](docs/adr/README.md).

## Crates

| Crate | Path | Owns | May depend on |
|---|---|---|---|
| `runnerd-core` | `crates/core` | config, API types, the socket client every client uses, scheduling, Docker and GitHub clients, caches, job history, sizing advice | anything; Docker and GitHub only behind the `host` feature, Tokio behind `host` or `client`, hyper behind `client` |
| `runnerd` | `crates/daemon` | the daemon binary: `init`, `serve`, `stop`, `doctor`, `check-config`, the Unix-socket API | `runnerd-core` with `host`; hyper for a shell's upgraded connection |
| `runnerd-cli` | `crates/cli` | the command-line client, binary `runnerctl`: `human` (text), `watch`, `tui` (full-screen dashboard, `tui` feature), `output` (the only printing) | `runnerd-core` with `client`, without `host`; `ratatui` for the TUI, `rustix` for the TUI and a shell's raw terminal |
| `runnerd-web` | `crates/web` | the web dashboard: pages, charts, live updates, its session | `runnerd-core` with `client`, without `host` |

`runnerd-core` modules:

- `api`, `config`: build without `host`. Every client reads these.
- `client`: the `client` feature. HTTP over the socket, timeouts and the event stream with its reconnects, for every
  client ([ADR 0030](docs/adr/0030-one-socket-client-in-core.md)).
- `docker`, `github`, `cache`, `isolation`, `record`, `scheduler`, `observe`, `sizing`, `store`: `host` only
  ([`crates/core/src/lib.rs`](crates/core/src/lib.rs)).

The web dashboard (`crates/web`) is a client like `runnerctl`: the daemon runs it in its process, but it reads and
writes only through the API on the Unix socket ([ADR 0020](docs/adr/0020-dashboard-is-an-api-client-on-loopback.md)).
The optional tray client (`crates/desktop`, [ADR 0034](docs/adr/0034-desktop-framework.md)) will be another.
[`.github/scripts/check-boundaries.sh`](.github/scripts/check-boundaries.sh) fails CI when any workspace crate other
than `runnerd` and `runnerd-core` builds `bollard`, `reqwest` or core's `host` feature.

## Request path

client → transport → principal → handler → core

1. A client sends JSON over the daemon's Unix socket (`$XDG_RUNTIME_DIR/runnerd.sock`).
2. The transport ([`crates/daemon/src/api.rs`](crates/daemon/src/api.rs)) reads the peer's credentials and turns
   them into an `api::Principal` in one extractor, `Authenticated`. Only the user the daemon runs as gets through.
3. Handlers take that principal, read `Handle::status` or the job store (`Handle::store`), or call `Control`
   (pause, resume, reseed, cache clean, remove records, open a shell, reload, shutdown). `GET /v1/events` streams
   the store's events as server-sent events. The config routes
   ([`crates/daemon/src/api/control.rs`](crates/daemon/src/api/control.rs)) validate a new config and hand the
   engine the parsed config with a `Commit` that writes the file; the engine runs the commit between ticks and runs
   with the config before its next tick, only if the file was written.
   `POST /v1/records/{job}/shell` ([`crates/daemon/src/api/records.rs`](crates/daemon/src/api/records.rs)) answers
   `101 Switching Protocols` and carries the shell's terminal over the same connection
   ([ADR 0031](docs/adr/0031-records-of-failed-jobs.md)).
4. The engine ([`crates/core/src/scheduler/engine.rs`](crates/core/src/scheduler/engine.rs)) owns all slot state.
   It publishes `api::Status` after every tick and command and takes commands only through `Control`: a command
   that needs slot state is a `Command` on a channel the engine answers between ticks, or while a tick waits for
   GitHub's listings and job reads, never in the middle of applying them
   ([ADR 0026](docs/adr/0026-commands-answered-while-a-tick-waits-for-github.md)).
   `engine.rs` holds `Engine`'s state, its construction and its loop; each submodule of
   [`engine/`](crates/core/src/scheduler/engine) adds one concern's methods: `recover` (startup), `tick` (polling
   and planning), `runners` (a slot's runner from start to removal), `images` (pulling the profiles' images),
   `caches` (reseeds, the mirror, the shared caches), `records` (records of failed jobs and the shells in them),
   `history` (what the store records), `control` (`Control` and the commands) and `status` (what it publishes).

A new transport (SSH, Tailscale) produces a `Principal` and reuses the handlers.

The dashboard's path is browser → loopback TCP → session check → `runnerd-web` → Unix socket → the path above. Its
session check ([`crates/web/src/auth.rs`](crates/web/src/auth.rs)) admits a request only with the session cookie, and
a write only with the dashboard's own `Origin`; the daemon then sees the dashboard as its own user.

## Job lifecycle

queued → slot → container → removed

1. Every `poll_interval` the engine asks GitHub for runners first, then queued jobs (`Engine::tick`; the order
   avoids starting a second runner for a job its runner just took), every repository at once, a few requests at a
   time.
2. `scheduler::plan`, a pure function, picks a free slot per queued job within the memory budget. A profile whose
   `image` Docker lacks starts nothing until the engine pulled it in the background
   ([ADR 0025](docs/adr/0025-published-runner-images-named-by-reference.md)).
3. `Engine::start` prepares the slot's volumes, registers a JIT runner (`GithubApi::jit_config`) and starts its
   container (`DockerApi::start`) under the profile's limits and isolation backend.
4. The runner takes one job and exits; the image's entrypoint then deletes the runner's credentials and every
   dot-file in its home and file in `/tmp` the job added or changed (`image/scrub.sh`). `Engine::collect_finished` sees the container stopped, removes it and the runner's
   registration, and frees the slot. A runner that took no job holds the repository back for a while. If the job
   failed and its profile has `record_failures`, the engine first commits the container to a record
   (`runnerd-record:<job>`) within `[records]`' limits, then removes it
   ([ADR 0031](docs/adr/0031-records-of-failed-jobs.md)).
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
| Slot and repository state | in memory, in `Engine`; rebuilt from container labels on restart, except paused repositories, which the job history keeps |
| Caches | Docker volumes named by `cache::Names`: `runnerd-<profile>-base`, `runnerd-<profile>-slot-<n>`, `runnerd-pnpm`, `runnerd-maven`, `runnerd-mirror` |
| Runner containers | labelled `runnerd.job`, `runnerd.profile`, `runnerd.slot`, `runnerd.repository`, `runnerd.runner-id` |
| Records of failed jobs | Docker images `runnerd-record:<job>`, labelled `runnerd.record` with the job's ID, repository, profile, name and when it was made; shells in them are containers labelled `runnerd.shell` |
| Budget | the systemd slice's `MemoryMax`, read with `systemctl` |
| Dashboard session | in memory, new each daemon run; the one-time login URL in `runnerd.web` next to the socket (0600) |
| Job history | SQLite, default `$XDG_STATE_HOME/runnerd/runnerd.db` (`store::Store`); jobs, runners, steps, samples, logs, events, cache sizes, paused repositories, what became of each record; times in ms |

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
- Only the store's writer thread writes the database; every write goes through `Store`'s queue, in order. The one
  exception is pruning, which only deletes, on its own connection, a batch per transaction
  ([ADR 0027](docs/adr/0027-pruning-runs-beside-the-writer.md)).
- Host samples reach subscribers when taken and the database a minute's worth at a time; reads include the ones not
  yet stored.
- A durable event (every kind but `job.sample`, `host.sample` and `log.line`) is stored in the same transaction as
  the change it announces and broadcast only after it commits, so its ID is a position a client can resume from
  with `Last-Event-ID`.
- The store keeps each profile's and repository's latest `profile.*` and `repository.*` event through pruning. A
  started engine announces what differs from them and every slot, so a client resuming after the last run's events
  ends where `/v1/status` is ([ADR 0029](docs/adr/0029-profile-events-and-what-a-restart-announces.md)).
- The engine does not start a runner for a queued job one of its runners holds, whatever GitHub's queue says.
- A reload never stops a running job: slots a new config removes go once their runners finish, and a profile starts
  no runner while it runs as many as its configured `slots` (`scheduler::Slot::Closed`).
- A forced pause cancels runs only of jobs this daemon's runners hold, and only of the paused repositories.
- `Control` records the paused repositories in the store, under the lock that changes them, and answers a pause or
  resume once the record commits, or fails it (`ControlError::NotRecorded`, 500) if it does not; the engine starts
  with the recorded ones paused, minus those its config lacks, so a paused repository gets no runner after a restart.
- Sizing advice (`GET /v1/sizing`, `sizing::advise`) is computed from the job history and never written by the
  daemon: a client applies it on request, after a confirmation, through `PUT /v1/config`
  ([ADR 0032](docs/adr/0032-sizing-advice-from-sampled-peaks.md)).
- The config file is replaced only by a config that validates against the slice's `MemoryMax`, by a temporary file
  renamed over it, and the engine runs a config only once its file is in place: no tick runs between the two.
- After a restart, the memory caps of adopted runners, seed containers and the mirror come from Docker, not the
  config, which a reload may have lowered since they started.
- No record keeps the JIT config, `_work/_temp`, a git config's `extraheader`, or any dot-file in the runner's home
  (but the Rust toolchain's `~/.rustup` and `~/.cargo/{bin,registry,git}`) or file in `/tmp` or `/var/tmp` the job
  added or changed: the runner image deletes them when the runner exits, and
  the engine commits no container whose changes still hold one (`record::secrets_left`). Checking and committing a
  container run off the engine's loop.
- A shell's container counts against the budget like a slot and is refused when the budget cannot hold it. It
  mounts the shared caches read-only, and its profile does not reseed while it is open. It is removed when its shell
  exits, its client goes away, after 8 hours, or when the daemon stops; `Engine::recover` removes any left over.
  A record with a shell open is never removed.
- The dashboard listens on loopback only, never logs its session token or login code, and serves every file its
  pages load; its Content-Security-Policy allows nothing else.
