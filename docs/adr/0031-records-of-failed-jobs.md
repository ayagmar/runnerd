# 0031. Records of failed jobs, without their secrets

## Context

A failed job's runner is gone by the time anyone looks: the container is removed once the runner exits, and what
the job left in its workspace goes with it. Rerunning the job with a debug step costs a full run and may not fail
the same way.

The runner container is still there, stopped, when the engine collects it (`Engine::collect_finished`), so Docker
can commit it to an image before removal. That image holds whatever the job wrote outside volumes: the workspace,
the runner's `_diag` logs, and files that carry secrets. The runner's registration (`.runner`, `.credentials`,
`.credentials_rsaparams`), the JIT config, `actions/checkout`'s token in `.git/config`, and the step scripts in
`_work/_temp`, which hold any `${{ secrets.* }}` a step's script names inline.

Committed images take disk next to the caches, and a shell in one is a container the budget must hold.

## Decision

A profile with `record_failures = true` keeps a failed job's runner as a record: the engine commits the stopped
container to `runnerd-record:<job>`, labelled `runnerd.record`, then removes the container. A job counts as failed
when its runner's output says `completed with result: Failed`. Records are listed from Docker's images, not the job
history, so a restart finds them.

**Secrets.** Two layers, so neither alone has to be right:

1. The entrypoint touches `/run/runnerd-since` before it starts `run.sh`. Once the runner exits and its user's
   processes are gone, it runs `runnerd-scrub` (`image/scrub.sh`) as root. The scrub deletes every dot-file or
   dot-directory of the runner's home, and every entry of `/tmp` and `/var/tmp`, whose ctime is newer than the
   marker. A change inside a dot-directory of the image copies the directory up, which gives it a new ctime, so it
   goes whole. That covers the runner's registration and credentials
   and whatever logins and keys a job's tools leave, whatever their names: `~/.gitconfig` with a token in an
   `insteadOf` URL, `~/.aws`, `~/.gnupg`, `~/.config/gcloud`, `/tmp/deploy.pem`. A job cannot set a ctime back.
   Tools a job installs there go too (`~/.local`, `~/.nvm`, `~/.dotnet`), except the Rust toolchain: `~/.rustup`,
   and `~/.cargo`'s `bin`, `registry` and `git`, which hold binaries, downloaded crates and git checkouts a
   workspace's `Cargo.lock` names anyway. `~/.cargo`'s `credentials.toml` and `config.toml` may hold a registry's
   token and go. A shell needs a toolchain to re-run a failed step, and Rust's is the one the runner image lacks
   that installs into home; another may join the list once shown to hold no credentials.
   It also deletes the JIT config's directory and `_work/_temp` (the step scripts, `GITHUB_ENV`), and every
   `extraheader` line in the workspace's git configs. Mount points, the cache layers, are other file systems and
   are left alone: they are not part of a record.
2. Before committing, the engine reads the container's changes (`docker diff`). It refuses to keep a container
   whose added or modified paths include anything under the runner's dot-files, `/tmp`, `/var/tmp`, `/run/runnerd`
   or `_work/_temp` (`record::secrets_left`), but the Rust toolchain's directories above. A refused record's reason
   names the first five paths and counts the rest. A runner image without the scrub, or a container killed
   before it ran, leaves no record; the job history says why.

Everything else the job changed outside volumes stays: the workspace, `_diag`, and anything else outside the
runner's dot-files and the temporary directories, such as `~/go`. A step that saves a secret
there, or into the workspace, leaves it in the record: runnerd does not search file contents. `_diag` stays: it has
the job's log and the job message, which the runner masks as it masks the log on GitHub.

**Who can read a record.** Anyone who can use Docker on the host (`docker run runnerd-record:<job>`), and any
client runnerd's socket accepts, which is only the user runnerd runs as. Through the socket, that user can open a
shell with the job's files and network access on runnerd's network. The dashboard shows the command, not a
terminal.

**Storage.** `[records]` bounds them: `max_total` (20G), `max_count` (10), `max_age` (7d), and `min_free` (10G) free
on Docker's disk. A record is made only if the disk keeps `min_free` after it (estimated at twice the container's
written bytes, since containerd keeps the layer's blob and its unpacked copy); older records go first to make room
for it under `max_count` and `max_total`, and records past `max_age` go at the next tick. A record with a shell open
is never removed. The job history keeps, per job, when its record was made, its size, why it was not made, and when
and why it went (`count`, `size`, `age`, `user`), past the record itself.

**Shells.** `POST /v1/records/{job}/shell` upgrades the request's own connection to a raw terminal
(`Upgrade: runnerd-tty`), so a shell needs no listener beyond the API socket and passes its principal check.

- The engine runs the shell's container from the record with the profile's memory cap and counts it in the budget
  like a slot. When the budget cannot hold it, the request fails with 409 and says so.
- The shell mounts the profile's snapshot and the shared pnpm and Maven caches read-only.
- While a shell is open, its profile does not reseed: a requested reseed and `caches --clean` of a shared cache
  answer 409, and the shared caches are not trimmed. A shell is refused while its profile reseeds or the shared
  caches are trimmed.
- The container goes when the shell exits, when the client goes away (the daemon sends the terminal a Ctrl-D, then
  removes it), after 8 hours, when the daemon stops, and, for one a crash left, when the next daemon starts.
- The terminal keeps the size it was opened with: v1 sends no resize.

**Off the loop.** Docker walks the container's files for `docker diff` and for its size, and commits it. Both happen
in tasks that wake the engine when they finish, so a large workspace does not hold up a tick. The free-space check
counts the expected size of the records still being committed. A daemon that stops mid-record is caught up at the
next start: a record image the history has no row for is noted as made, and a stopped runner whose failed job got no
record becomes its record then, whether the daemon stopped while keeping it or was not running when it stopped
(amended by [0091](0091-a-signal-leaves-running-jobs-to-the-next-start.md); it was noted as skipped before).

## Consequences

A failed job costs a commit, a second or two for a few hundred MB, and its disk until a limit or `runnerctl records
rm` takes it. The record holds what the job changed outside volumes only: the slot's caches, the tool cache and the
job's Docker images show the profile's snapshot in a shell, not what the job left there.

`runnerd` depends on hyper directly to take the upgraded connection, and `runnerctl` on rustix's `termios` to put
its terminal in raw mode for the shell.

The runner image's entrypoint no longer `exec`s the runner: it waits for it, then scrubs. `run.sh` passes on no
signal, so the entrypoint sends `docker stop`'s TERM to the runner's listener, which cancels the job. Once `run.sh`
exits, the entrypoint sends the runner user's processes left (a Gradle daemon) TERM, gives them 2 s, then KILL, so
nothing writes after the scrub.
The runner gives a cancelled step up to 10 s (SIGINT, then SIGTERM after 7.5 s). A `docker stop` with its default
10 s timeout can therefore kill the container before the scrub. The guard then refuses it, and a cancelled job is
not recorded anyway.
