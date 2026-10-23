# runnerd

runnerd runs ephemeral GitHub Actions runners on a Linux host: one fresh container per job, from one pool shared by the repositories you configure, kept inside a memory and CPU budget.

It is in early development: the daemon schedules runners, records every job, serves a JSON API and a web dashboard, and `runnerctl` reads and controls it from a terminal.

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
curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd --version v0.0.5
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
  privileged: `.deb` packages, the AUR package `sysbox-ce-bin` on Arch Linux, or a build from source.
- A systemd slice whose `MemoryMax` is the budget for all runners.
- The [GitHub CLI](https://cli.github.com), logged in with the `repo` scope.

`runnerd doctor` checks each of these, the slice the config names and that the config fits in it, and prints the fix
for what is missing. It reads `~/.config/runnerd/runnerd.toml` when it exists; `runnerd doctor <file>` checks another.

## Running it as a service

The unit `runnerd init` writes:

```ini
[Service]
Type=exec
ExecStart="/home/dana/.local/bin/runnerd" serve --config "/home/dana/.config/runnerd/runnerd.toml"
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now runnerd   # start now and at login
loginctl enable-linger "$USER"          # keep it running while you are logged out
journalctl --user -u runnerd -f         # its log
systemctl --user restart runnerd        # after editing runnerd.toml by hand
systemctl --user stop runnerd           # waits for running jobs, up to systemd's stop timeout
```

Runner containers live in Docker, not in the unit: when runnerd stops or crashes, the next start adopts the ones still
running. A config saved from the dashboard or `PUT /v1/config` applies without a restart.

## Runner image

[`image/`](https://github.com/ayagmar/runnerd/tree/main/image) builds the container every job runs in: the GitHub Actions runner, its own Docker Engine, and one Dockerfile stage per profile on a common base (`base`, `jvm`, `node`). Each release publishes every stage to `ghcr.io/ayagmar/runnerd-runner:<stage>-<version>` for linux/amd64, with a build provenance attestation. A profile names the image its runners run with the `image` key (since 0.0.5):

```toml
[profiles.jvm]
image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.0.5"
```

runnerd pulls the image before the profile's first runner and logs the pull's progress; `runnerd doctor` says whether each image is present or can be pulled. Keep the tag at the daemon's version: the image's scripts and the daemon change together, and doctor warns when they differ. To check where an image came from:

```sh
gh attestation verify oci://ghcr.io/ayagmar/runnerd-runner:jvm-0.0.5 --repo ayagmar/runnerd
```

runnerd 0.0.4 does not know `image`; pull and retag the published image, then keep `image_stage = "jvm"`: `docker pull ghcr.io/ayagmar/runnerd-runner:jvm-0.0.4 && docker tag ghcr.io/ayagmar/runnerd-runner:jvm-0.0.4 runnerd-runner:jvm`.

To run your own changes to `image/`, build the stage from a clone and set `image_stage` instead of `image`; runners then run the local image `runnerd-runner:<image_stage>`, which runnerd never pulls:

```sh
docker build --target jvm -t runnerd-runner:jvm image
```

Rebuilds install the same versions: Ubuntu packages come from one snapshot date and every download is checked against its SHA-256 ([ADR 0023](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0023-runner-image-pinned-to-a-snapshot-date.md)).

Documentation: <https://ayagmar.github.io/runnerd/>

## How it runs

`runnerd serve` reads `$XDG_CONFIG_HOME/runnerd/runnerd.toml`, or the file `--config` names. It reads the `gh` CLI's token at start (and again when GitHub rejects it), then every `poll_interval` (`[host]`, default `15s`) asks GitHub for each repository's queued jobs. A job is served when every label in its `runs-on` is one of its repository's `labels`. For each such job it registers a just-in-time runner on the repository and starts it in a free slot of the repository's profile, if the slice's `MemoryMax` still has room for the profile's `memory`. The runner takes one job, its container exits, and runnerd removes it; a runner that got no job within 5 minutes is removed. Jobs that cannot be served yet wait in GitHub's queue.

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
  with its record;
- the bytes the Docker Hub mirror served it, from the mirror's access log;
- its log, up to 50M, as GitHub shows it, with the secrets the runner masked still masked, for 30 days.

A finished job's record goes after 90 days (`history.jobs`), with its steps and runner. The database file does not
shrink when rows go; SQLite reuses the space.

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
in [`crates/core/src/api.rs`](https://github.com/ayagmar/runnerd/blob/main/crates/core/src/api.rs) and its modules. Times are seconds since the Unix epoch, sizes
bytes, CPU millicores.

| Request | Response |
|---|---|
| `GET /v1/status` | profiles and their slots, running runners, repositories with their queued jobs, the memory budget |
| `POST /v1/repositories/{owner}/{name}/pause?force=` | the repository gets no new runners; `409` naming the job while it runs one, unless `force=true`, which cancels the job's run |
| `POST /v1/repositories/{owner}/{name}/resume` | `204` |
| `POST /v1/repositories/pause?force=`, `POST /v1/repositories/resume` | the same for every configured repository at once; a pause while any of them runs a job is refused with `409` naming the jobs, and pauses none, unless `force=true` |
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
| `GET /v1/host/samples?since=` | the slice's CPU and memory over time, the last 24 hours at most |
| `GET /v1/stats?repository=&since=&until=&bucket=day` | finished jobs per repository and day (or `hour`), 744 buckets back at most: counts, queue and run time, CPU, memory, mirror bytes |
| `GET /v1/caches`, `GET /v1/caches/history?since=` | cache volume sizes and their limits, now and over time |
| `GET /v1/events?job=&logs=` | server-sent events: `job.queued`, `job.started`, `job.step`, `job.finished`, `slot.changed`, `profile.changed` (a reload added or changed it, or it started or stopped reseeding), `profile.removed` (a reload removed it and its last runner is gone), `budget.changed`, `repository.changed` (paused or resumed, its queue, or a reload changed or added it), `repository.removed` (a reload removed it), `cache.reseeded`, and live `job.sample`, `host.sample` and (with `logs=true`) `log.line` |

A forced pause cancels the run of each of the repository's running jobs (GitHub cancels runs, not single jobs, so
the run's other jobs are cancelled too) and lets the runner report it cancelled, which takes under a minute; a runner
still running 90 s later is removed. Its runner registration is deleted once GitHub lets go of it.

A pause lasts until a resume, across daemon restarts: the job history records the paused repositories, and a
restarted daemon starts no runner for them. If the history cannot record a pause or resume, it still holds until
the daemon restarts, and the request fails with `500` saying a restart undoes it. A reload that removes a paused repository forgets its pause; added back,
it starts active.

A new config takes effect without a restart, and running jobs keep running. A removed profile, or slots past a lowered
`slots`, stay until their runners finish, and no runner starts in a profile while it runs as many as its `slots`.
A runner still waiting for a job of a removed repository, or of one moved to another profile, is removed.
Runners started afterwards get the new limits, image and labels. `history.database` changes only at the next start.

Every event but the live ones has an ID; send the last one seen as `Last-Event-ID` and the stream resumes after it,
across daemon restarts. Live events are not stored: a client that missed some reads the job's samples or log.
`slot.changed`, `profile.changed`, `budget.changed` and `repository.changed` carry the state as it is after a change,
not each transition: a pause and a resume that land before the daemon publishes its status send nothing, since the
state did not change. Applying `profile.*` and `slot.changed` to the profiles of a status, and `repository.*` to its
repositories, keeps them equal to what `/v1/status` returns.

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
- each job's phases, its steps on one time axis, its CPU and memory against the caps its container ran with (the
  profile's limits as configured now for jobs recorded before runnerd kept them), and its log;
- the history, filtered by repository and outcome, with 30 days of jobs per day, run and wait times and memory peaks;
- the cache volumes with their bounds and their sizes over 30 days, with a button to reseed a snapshot or empty a
  cache;
- `runnerd.toml` in an editor that checks it against the slice's `MemoryMax` as you type and saves and reloads it
  only when it fits, next to the doctor's checks;
- pause and resume for each repository or all of them; a pause while a job runs asks first, and confirming cancels
  the job.

Views of the same thing are linked. On the overview, a slot's block opens its job, and pointing at a block, card,
running job or repository, focusing it or tapping it lights up the others and says above the bar what the block
holds, including what is free. On a job, a step marks its stretch of the memory and CPU charts. On the history, a
day (pointed at, or stepped to with the arrow keys) is marked on all three charts and its numbers replace the 30
days' in the headline.

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

## runnerctl

`runnerctl` is the daemon's command-line client. It talks to the socket only (`--socket` to pick another), prints
aligned text, coloured on a terminal unless `NO_COLOR` is set, and with `--json` prints the API's response type
unchanged.

```console
$ runnerctl status
runnerd 0.0.5  4.5G of 33G reserved in runnerd.slice

PROFILE  SLOT  STATE  REPOSITORY      RUNNER                       UP
jvm         1  busy   halyard/ledger  runnerd-5f2a91c0-jvm-1-k3j9  21s
jvm         2  free

REPOSITORY          PROFILE  STATE   QUEUED  LABELS
halyard/ledger      jvm      active       0  self-hosted,linux,x64,jvm
halyard/storefront  node     paused       2  self-hosted,linux,x64,node
```

| Command | What it shows or does |
|---|---|
| `runnerctl status` | the memory budget, every slot and its runner, the repositories and their queues |
| `runnerctl jobs [--repo halyard/ledger] [--failed] [--since 7d]` | jobs newest first: result, time waited and run, sampled memory peak; `--state`, `--conclusion`, `--before`, `--limit` too |
| `runnerctl job <id>` | one job: timeline, steps with their durations, CPU, memory, network, log size |
| `runnerctl logs <id> [-f]` | its log; `-f` follows it until the job's record is final, waiting out a daemon restart |
| `runnerctl watch` | slots, running jobs with their current step and last sample, queued and finished jobs, redrawn as events arrive; one line per event when not on a terminal, one JSON event per line with `--json` |
| `runnerctl tui` | the full-screen dashboard below |
| `runnerctl pause <repo> [--force]`, `resume <repo>` | the repository gets no new runners, also after a restart; refused while it runs a job, naming the job, unless `--force` cancels the job's run and removes its runner; and back |
| `runnerctl pause --all [--force]`, `resume --all` | the same for every configured repository; refused while any runs a job, pausing none |
| `runnerctl reseed <profile>` | makes slot 1's layers the profile's snapshot once its slots are free, and waits for it (`--no-wait`) |
| `runnerctl caches [--clean <volume>]` | the cache volumes, their sizes and limits; or empties a slot layer or shared cache no runner uses |
| `runnerctl config show`, `config check [file]` | the config the daemon runs with; a file checked against the daemon's budget, as `runnerd check-config` does |
| `runnerctl doctor` | the daemon's host checks, or `runnerd doctor` run here when the daemon is not running |
| `runnerctl web` | opens the web dashboard with the one-time login URL the daemon writes to `$XDG_RUNTIME_DIR/runnerd.web` (`--no-open` prints it) |

Memory and CPU come from samples every 2 s, so a peak is the highest sample: a spike shorter than that does not show.

### runnerctl tui

A full-screen dashboard for hosts reached over SSH, where the web dashboard is not open. It shows what the web
dashboard does:

- the budget as one bar, a block per slot as wide as its memory and filled as far as its job uses it;
- a card per slot with the job, its step, run time, and memory and CPU sparklines;
- the slice's memory and CPU, the repositories, and the latest jobs;
- the history, filtered by outcome and repository;
- a job: its timeline, its steps as bars on one time axis, memory and CPU against its container's caps, and its log,
  followed while the job runs.

Profiles take the dashboard's colours in config order, mint first and pink second. `NO_COLOR` leaves bold, dim and
block glyphs. At 80×24 the slots become one line each and the charts drop out first.

| Key | |
|---|---|
| `1` `2` | overview, history |
| `tab`, `j` `k` (or arrows) | the overview's pane (slots, repositories, latest jobs); move in it, or scroll a log |
| `enter`, `esc` | open the selected job; back |
| `p` `r` | pause or resume the selected repository; a pause refused while it runs a job asks before forcing it |
| `P` `U` | pause or resume every repository, asking the same way |
| `R` | reseed the selected slot's profile |
| `c` `s` `]` `[` | history: cycle the outcome and repository filters, next and previous page |
| `G` `g` | follow the log, or go to its start |
| `?` `q` | all keys; quit |
| `ctrl-z` | suspend; `fg` brings it back, redrawn |

SIGINT, SIGTERM and SIGHUP quit and SIGTSTP suspends, each after giving the terminal back. When the daemon goes
away the header says `reconnecting`. Once it is back, the event stream resumes after the last event read, or the
state is read again if none was; an open job's log carries on from its next line. Build without it with `cargo build -p runnerd-cli --no-default-features`.

## Config

One file, `runnerd.toml`, lists the repositories to serve, the runner profile each one uses, the shared caches and the systemd slice whose limits form the budget. `runnerd init` writes a starter one; [`examples/runnerd.toml`](https://github.com/ayagmar/runnerd/blob/main/examples/runnerd.toml) has every setting.

```sh
runnerd check-config              # ~/.config/runnerd/runnerd.toml
runnerd check-config other.toml
```

This reads the slice's `MemoryMax` from systemd and fails when `slots × memory` over all profiles, plus the Docker Hub mirror, does not fit in it. A slice without a unit file has no `MemoryMax`; the check passes with a warning, since nothing would hold the runners to their caps. To check against a size without systemd, pass it: `--memory-max 33G`.

Sizes take a binary unit (`K`, `M`, `G`, `T`), as Docker and systemd use them.

`isolation = "unsafe-dind"` runs privileged containers, so a job can take over the host. It is refused unless the `[host]` table also sets `allow_unsafe = true`.

## Building

```sh
cargo build --release
```

The toolchain is pinned in `rust-toolchain.toml`. Before sending a change, run the checks listed under "The gate" in
[AGENTS.md](https://github.com/ayagmar/runnerd/blob/main/AGENTS.md); CI runs the same. [CONTRIBUTING.md](https://github.com/ayagmar/runnerd/blob/main/CONTRIBUTING.md) explains which CI jobs a change triggers,
and [ARCHITECTURE.md](https://github.com/ayagmar/runnerd/blob/main/ARCHITECTURE.md) where the code lives.

The tests that start real containers are ignored by default. They need Docker with sysbox and the images:

```sh
docker build --target jvm -t runnerd-runner:jvm image
docker build --target base -t runnerd-runner:base image
cargo test -p runnerd-core --test docker_smoke --test cache_reseed -- --ignored
```

## License

MIT
