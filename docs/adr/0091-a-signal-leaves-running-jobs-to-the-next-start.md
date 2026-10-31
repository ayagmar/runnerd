# 0091. A signal leaves running jobs to the next start, and a stop ends within 30 s

Status: **Accepted** (2026-10-31). Amends [0031](0031-records-of-failed-jobs.md): the next start makes the
record of a failed job whose stopped runner no daemon kept.

## Context

systemd stops a user service with SIGTERM and kills it 90 s later (`TimeoutStopSec`'s default). On one host, 13 of
26 stops in a week ended that way, on shutdowns and on restarts after installs. Two causes:

- **Docker gone, a loop.** During a machine's shutdown Docker stops first. Each stop tick planned to remove the idle
  runners: `stop` deleted the runner on GitHub, logged "removed idle", and `release` returned on the failed removal
  without freeing the slot, as ADR 0076 wants. The next tick, 5 s later, did the same again, and the loop ended only
  once every slot was free, which never happened. The same held for a busy runner whose `inspect` failed. Six stops
  logged 30 to 52 "removed idle" lines for the same 4 runners.
- **A running job.** SIGTERM meant "stop once running jobs finish". A job that ran past the timeout was killed
  with the daemon, and the next start adopted its container anyway. Six stops went this way.

Even a clean stop removed idle runners one after the other, about 12 s each; five took 49 to 58 s. One Docker
request can take 120 s before it times out, and one GitHub request 10 s, so no per-call timeout bounds a stop.

Leaving every runner to the next start would end a stop fastest, but an idle runner left registered stays online
and takes one more job with no daemon to watch it, record it or serve its caches. After an install, the next start
would also adopt idle runners started with the old release's container spec, and they would take the first jobs.

Recovery (`Engine::recover`) already adopts every runner container left running, idle ones as busy until the first
listing, and a running seed container as its profile's reseed; it removes stopped ones, and those of no slot.

## Decision

- **Three kinds of stop**, from least to most urgent (`engine::Shutdown`); a later request only makes a stop more
  urgent:
  - `Graceful`, `runnerd stop` and `POST /v1/shutdown`: start nothing new, wait for running jobs, then remove every
    runner and stop the mirror. No deadline: waiting is what it is for.
  - `Detach`, the first SIGTERM or SIGINT: run no more ticks. Idle and warm runners are removed, best effort and
    all at once within the deadline: each is deregistered first, a few GitHub requests at a time, then their
    containers are removed together, as each removal waits for its container to stop. GitHub's `Busy` keeps a runner
    that just took a job, and one whose deregistration or removal fails stays. Busy runners and the mirror stay for
    the next start to adopt.
  - `Force`, `runnerd stop --force` or a second signal: kill running jobs and remove every runner.
- **A deadline.** `Engine::run` returns `STOP_DEADLINE` (30 s) after a detach or a forced stop was asked for,
  wherever it is: in a tick waiting for Docker or GitHub, or in recovery. It returns normally, so the daemon still
  closes and flushes the history; nothing calls `process::exit`.
- **Docker gone ends the stop.** Once Docker does not answer a call a stop needs (a removal, an inspect of a runner)
  at all, its socket gone or refusing connections (`docker::is_unreachable`), the stop goes on as a detach: it calls
  no more stops and leaves the rest to the next start. An error Docker answers with, or a timeout, is tried again at
  the next tick, as while scheduling: one busy moment of Docker's must not turn `runnerd stop` into a detach that
  leaves runners behind. While scheduling, a slot still stays held until Docker confirms its container's removal (ADR 0076): freeing it on a failed removal could
  start a second runner on the volumes of one still running.
- **Runners carry their release.** A runner container's `runnerd.spec` label names the release that started it. The
  first tick after an install replaces an idle runner of another release, or of one before the label, as it replaces
  one a reload left outdated; a busy one finishes its job. This covers idle runners a detach could not remove.
- **"removed idle" is logged after the removal**, not before.
- **What else holds a stop:**
  - A record being made (a failed job's container committed) is waited for in every kind of stop, within the
    deadline for a detach or a forced stop.
  - A failed job's runner that stopped and was not yet kept, because it ended just before the stop, while no daemon
    ran, or while a record was cut off by the deadline, is kept at the next start: recovery makes its record from the
    stopped container (`keep_left_record`) instead of noting the record lost, as `release` would have.
  - A reseed is not waited for. Its seed container runs on, and the next start adopts it as its profile's reseed. A
    reseed that finished just before the stop is still collected, so its event and the slots' dropped layers are
    recorded.
  - A check of the shared caches is abandoned: it only measures volumes and removes those over their limit, and the
    next start checks first.
- **Nothing more while stopping.** A detaching engine answers no commands; one sent meanwhile fails as it would once
  the daemon is gone. Auto sizing stops looking once any stop is asked for.

## Consequences

- `systemctl --user stop runnerd` and machine shutdowns end in seconds, about one container removal's time, and
  running jobs carry on. Each busy runner exits after its job, so this machine takes no new job until the next start,
  except on an idle runner the detach could not remove, which stays online and can take one. Between the stop
  and the next start, the caches the daemon serves itself do not answer: GitHub Actions' cache (ADR 0082) and the
  build cache's sockets (ADR 0052) are gone, so a job running then restores and saves nothing there. The registry
  mirror stays up. The next start re-attaches each adopted runner's build cache socket.
- A runner that finishes its job while no daemon runs stays a stopped container until the next start removes it, or
  keeps it as its failed job's record, and its job's end is recorded from GitHub then, as after a crash.
- To leave nothing behind, as before another daemon uses this Docker, run `runnerd stop`, not `systemctl stop`.
  CONTRIBUTING's sandbox procedure says so.
- The unit needs no `TimeoutStopSec`: a stop ends within 30 s plus the history's flush. `KillMode` does not matter,
  as runner containers live in Docker's cgroups, not the unit's.
- A stop that hits the deadline can leave a runner registered with no container, or a start half done; the next
  start's first listing deletes such registrations, and recovery adopts or removes the container by its labels.
