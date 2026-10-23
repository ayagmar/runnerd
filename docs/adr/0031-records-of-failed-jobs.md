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

1. The runner image runs `runnerd-scrub` (`image/scrub.sh`) as root once `run.sh` exits, before the container
   stops. It deletes the JIT config's directory, the runner's registration and credentials, the usual credential
   files in the runner's home, `_work/_temp`, and every `extraheader` line in the workspace's git configs.
2. Before committing, the engine reads the container's changes (`docker diff`) and refuses to keep a container
   whose added or modified paths include any of those files (`record::secrets_left`). A runner image without the
   scrub, or a container killed before it ran, leaves no record; the job history says why.

What a job writes to its own workspace on purpose is the job's to keep or not; runnerd does not search file
contents. `_diag` stays: it has the job's log, and the runner masks secrets there as it does on GitHub.

**Storage.** `[records]` bounds them: `max_total` (20G), `max_count` (10), `max_age` (7d), and `min_free` (10G) free
on Docker's disk. A record is made only if the disk keeps `min_free` after it (estimated at twice the container's
written bytes, since containerd keeps the layer's blob and its unpacked copy); older records go first to make room
for it under `max_count` and `max_total`, and records past `max_age` go at the next tick. A record with a shell open
is never removed. The job history keeps, per job, when its record was made, its size, why it was not made, and when
and why it went (`count`, `size`, `age`, `user`), past the record itself.

**Shells.** `POST /v1/records/{job}/shell` upgrades the request's own connection to a raw terminal
(`Upgrade: runnerd-tty`), so a shell needs no listener beyond the API socket and passes its principal check. The
engine runs the shell's container from the record with the profile's memory cap and counts it in the budget like a
slot; when the budget cannot hold it, the request fails with 409 and says so. The container goes when the shell
exits, when the client goes away (the daemon sends the terminal a Ctrl-D, then removes it), when the daemon stops,
and, for one a crash left, when the next daemon starts.

## Consequences

A failed job costs a commit, a second or two for a few hundred MB, and its disk until a limit or `runnerctl records
rm` takes it. The record holds what the job changed outside volumes only: the slot's caches, the tool cache and the
job's Docker images show the profile's snapshot in a shell, not what the job left there.

`runnerd` depends on hyper directly to take the upgraded connection, and `runnerctl` on rustix's `termios` to put
its terminal in raw mode for the shell. The runner image's entrypoint no longer `exec`s the runner: it waits for it,
forwarding `docker stop`'s signal, so the scrub runs after it.
